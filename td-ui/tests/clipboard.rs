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
use td_ui::clipboard::{Incoming, Outgoing, MAX_BYTES, MAX_IMAGE_BYTES};

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

/// Feeds `length` bytes from a writer thread into a picture paste,
/// stepped at a fixed clock until it ends: its bytes, or the failure.
fn feed_picture(length: usize) -> io::Result<Vec<u8>> {
    let (mut incoming, mut peer) = Incoming::begin_image(0).unwrap();
    // Three bytes first, read alone, so a Vec left to double from its
    // reads would grow from an odd size past the budget.
    let writer = std::thread::spawn(move || {
        let head = length.min(3);
        if peer.write_all(&vec![0xa5u8; head]).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
        let chunk = vec![0xa5u8; 1 << 20];
        let mut left = length - head;
        while left > 0 {
            let n = left.min(chunk.len());
            if peer.write_all(&chunk[..n]).is_err() {
                return;
            }
            left -= n;
        }
    });
    let start = std::time::Instant::now();
    let ended = loop {
        match incoming.step(1) {
            Ok(true) => break Ok(()),
            Ok(false) => {}
            Err(e) => break Err(e),
        }
        assert!(start.elapsed() < Duration::from_secs(20), "stalled");
        std::thread::sleep(Duration::from_millis(1));
    };
    // Finishing drops the reader, so a writer refused mid-way stops.
    let bytes = ended.and_then(|()| incoming.finish_bytes());
    writer.join().unwrap();
    bytes
}

#[test]
fn a_picture_arrives_whole_up_to_its_budget_in_no_more_room_than_that() {
    let bytes = feed_picture(MAX_IMAGE_BYTES).unwrap();
    assert_eq!(bytes.len(), MAX_IMAGE_BYTES);
    assert!(bytes.capacity() <= MAX_IMAGE_BYTES, "{}", bytes.capacity());
    assert!(bytes.iter().all(|&b| b == 0xa5));
    let over = feed_picture(MAX_IMAGE_BYTES + 1).unwrap_err();
    assert_eq!(over.to_string(), "clipboard paste byte budget");
    // Bytes that are not UTF-8 are a picture's, and text's failure.
    let (mut incoming, mut peer) = Incoming::begin_image(0).unwrap();
    peer.write_all(&[0xff, 0xfe]).unwrap();
    drop(peer);
    assert!(incoming.step(1).unwrap());
    assert_eq!(incoming.finish_bytes().unwrap(), [0xff, 0xfe]);
    let (mut incoming, mut peer) = Incoming::begin(0).unwrap();
    peer.write_all(&[0xff, 0xfe]).unwrap();
    drop(peer);
    assert!(incoming.step(1).unwrap());
    assert!(incoming.finish().is_err());
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

#[test]
fn owned_text_refuses_each_allocation_and_retains_credit_until_the_last_send() {
    use td_alloc::{with_reservation_limit, Budget, BudgetError};
    use td_ui::clipboard::{OwnedText, TextError};
    let root = Budget::new(4 * MAX_BYTES).unwrap();
    let empty = root.used();
    let payload = "payload".repeat(30);
    let expected = format!("é\n{payload}");
    let pieces = ["é", "\n", payload.as_str()];
    for allowed in 0..=2 {
        let result = with_reservation_limit(allowed, || {
            OwnedText::from_pieces(&root, pieces.into_iter(), MAX_BYTES)
        });
        if allowed < 2 {
            assert!(matches!(
                result,
                Err(TextError::Resource(BudgetError::Allocation))
            ));
            assert_eq!(root.used(), empty);
        } else {
            let text = result.unwrap();
            assert_eq!(text.as_str(), expected);
            let held = root.used();
            let second = with_reservation_limit(0, || text.try_clone()).unwrap();
            assert_eq!(root.used(), held);
            let (mut reader, writer) = UnixStream::pair().unwrap();
            let mut send =
                with_reservation_limit(0, || Outgoing::begin_owned(writer.into(), second, 0))
                    .unwrap();
            drop(text);
            assert_eq!(root.used(), held);
            assert!(with_reservation_limit(0, || send.step(1)).unwrap());
            let mut received = String::new();
            reader.read_to_string(&mut received).unwrap();
            assert_eq!(received, expected);
            assert_eq!(root.used(), held, "completed owner still retains its bytes");
            drop(send);
            assert_eq!(root.used(), empty);
        }
    }
    let large = "x".repeat(MAX_BYTES + 1);
    assert!(matches!(
        with_reservation_limit(0, || OwnedText::copy(&root, &large)),
        Err(TextError::TooLong)
    ));
    let pressure = root.charge(root.limit() - root.used()).unwrap();
    assert!(matches!(
        OwnedText::copy(&root, &"x".repeat(129)),
        Err(TextError::Resource(BudgetError::Limit))
    ));
    drop(pressure);
    assert!(OwnedText::copy(&root, "recovered").is_ok());
    assert_eq!(root.used(), empty);
}

#[test]
fn owned_send_timeout_broken_peer_and_cancel_release_their_credit() {
    use td_alloc::{with_reservation_limit, Budget};
    use td_ui::clipboard::OwnedText;
    let root = Budget::new(4 * MAX_BYTES).unwrap();
    let empty = root.used();
    for mode in 0..3 {
        let text = OwnedText::copy(&root, "payload").unwrap();
        let (reader, writer) = UnixStream::pair().unwrap();
        let mut send = Outgoing::begin_owned(writer.into(), text, 0).unwrap();
        if mode == 0 {
            assert!(with_reservation_limit(0, || send.step(5000)).is_err());
            drop(send);
        } else if mode == 1 {
            drop(reader);
            assert!(with_reservation_limit(0, || send.step(1)).is_err());
            drop(send);
        } else {
            with_reservation_limit(0, || send.cancel()).unwrap();
        }
        assert_eq!(root.used(), empty);
    }
}

// Keep receiver validation independent of the sending owner's code generation.
#[inline(never)]
fn receive_benchmark_bytes(reader: &mut UnixStream) -> usize {
    static EXPECTED: [u8; 65536] = [b'x'; 65536];
    let mut count = 0;
    let mut chunk = [0u8; 65536];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return count,
            Ok(n) => {
                count += n;
                assert_eq!(&chunk[..n], &EXPECTED[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return count,
            Err(e) => panic!("read: {e}"),
        }
    }
}

#[test]
#[ignore]
fn owned_send_benchmark() {
    use std::hint::black_box;
    use td_alloc::Budget;
    use td_ui::clipboard::OwnedText;
    let case = std::env::var("TD_SEND_BENCH").unwrap();
    let (size, rounds) = if case == "short" {
        (80, 20_000)
    } else {
        (MAX_BYTES, 600)
    };
    let source = "x".repeat(size);
    let budget = Budget::new(4 * MAX_BYTES).unwrap();
    let cpu_ns = || -> u64 {
        std::fs::read_to_string("/proc/thread-self/schedstat")
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    let cpu_started = cpu_ns();
    let started = std::time::Instant::now();
    let mut count = 0;
    for _ in 0..rounds {
        let text = OwnedText::copy(&budget, black_box(&source)).unwrap();
        let (mut reader, writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let mut send = Outgoing::begin_owned(writer.into(), text, 0).unwrap();
        let mut received = 0;
        loop {
            let done = send.step(1).unwrap();
            received += receive_benchmark_bytes(&mut reader);
            if done {
                break;
            }
        }
        assert_eq!(received, size);
        count += black_box(received);
    }
    let cpu_ms = (cpu_ns() - cpu_started) as f64 / 1_000_000.0;
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    let rss = std::fs::read_to_string("/proc/self/status").unwrap();
    let peak = rss.lines().find(|l| l.starts_with("VmHWM:")).unwrap();
    println!("send-bench {case} ms={elapsed} cpu_ms={cpu_ms} bytes={count} {peak}");
}

#[test]
fn inline_text_needs_no_allocation_and_long_replacements_keep_both_charges() {
    use td_alloc::{with_reservation_limit, Budget};
    use td_ui::clipboard::{OwnedText, TextError};
    let root = Budget::new(3 * MAX_BYTES).unwrap();
    let empty = root.used();
    let short = with_reservation_limit(0, || OwnedText::copy(&root, "é hello")).unwrap();
    let clone = with_reservation_limit(0, || short.try_clone()).unwrap();
    assert_eq!(clone.as_str(), "é hello");
    assert_eq!(root.used(), empty, "inline values allocate no backing");
    let payload = "x".repeat(MAX_BYTES);
    let old = OwnedText::copy(&root, &payload).unwrap();
    let old_usage = root.used();
    let replacement = OwnedText::copy(&root, &payload).unwrap();
    assert!(root.used() >= old_usage + MAX_BYTES);
    let pressure = root.charge(root.limit() - root.used()).unwrap();
    assert!(matches!(
        OwnedText::copy(&root, &payload),
        Err(TextError::Resource(_))
    ));
    assert_eq!(old.as_str(), payload);
    assert_eq!(replacement.as_str(), payload);
    drop(pressure);
    drop(replacement);
    assert_eq!(root.used(), old_usage);
    drop(old);
    assert_eq!(root.used(), empty);
}
