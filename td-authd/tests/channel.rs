#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::process::{Child, Command, Stdio};

fn uid() -> u32 {
    std::fs::metadata("/proc/self").unwrap().uid()
}

struct Fixture(Child);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(stream: UnixStream, mode: &str) -> Fixture {
    Fixture(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!(
                    "{}::peer_fixture",
                    module_path!().split_once("::").unwrap().1
                ),
                "--nocapture",
            ])
            .env("TD_AUTH_CHANNEL_FIXTURE", mode)
            .stdin(Stdio::from(OwnedFd::from(stream)))
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

fn connected(mode: &str) -> (Channel, Fixture) {
    let (left, right) = UnixStream::pair().unwrap();
    let child = spawn(right, mode);
    (Channel::connect(left, uid(), uid()).unwrap(), child)
}

fn wait_closed(stream: &mut UnixStream) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let _ = stream.read(&mut [0u8]);
}

#[test]
fn peer_fixture() {
    let Ok(mode) = std::env::var("TD_AUTH_CHANNEL_FIXTURE") else {
        return;
    };
    let mut stream = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
    if mode == "delegate-before-greeting" {
        let mut child = spawn(stream, "echo");
        assert!(child.0.wait().unwrap().success());
        return;
    }
    if mode == "replacement" {
        sys::prepare(&stream).unwrap();
        stream.write_all(&[0, 0, 0, 1, b'X']).unwrap();
        wait_closed(&mut stream);
        return;
    }
    let mut channel = Channel::connect(stream.try_clone().unwrap(), uid(), uid()).unwrap();
    match mode.as_str() {
        "worker" => {
            std::thread::spawn(move || {
                while let Ok(bytes) = channel.receive() {
                    if channel.send(&bytes).is_err() {
                        break;
                    }
                }
            })
            .join()
            .unwrap();
        }
        "echo" => {
            while let Ok(bytes) = channel.receive() {
                if channel.send(&bytes).is_err() {
                    break;
                }
            }
        }
        "swap" => {
            assert_eq!(channel.receive().unwrap(), b"fork");
            let mut child = spawn(stream.try_clone().unwrap(), "replacement");
            assert!(child.0.wait().unwrap().success());
        }
        "dead" => {
            assert_eq!(channel.receive().unwrap(), b"die");
            stream.write_all(&[0, 0, 0, 1, b'X']).unwrap();
        }
        "oversize" => {
            assert_eq!(channel.receive().unwrap(), b"oversize");
            stream.write_all(&4097u32.to_be_bytes()).unwrap();
            wait_closed(&mut stream);
        }
        "partial" => {
            assert_eq!(channel.receive().unwrap(), b"partial");
            stream.write_all(&[0, 0]).unwrap();
            wait_closed(&mut stream);
        }
        "trickle" => {
            assert_eq!(channel.receive().unwrap(), b"trickle");
            for byte in [0u8, 0, 0, 4, b'a', b'b', b'c', b'd'] {
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(800));
            }
            wait_closed(&mut stream);
        }
        _ => panic!("bad fixture mode"),
    }
}

#[test]
fn distinct_process_sender_is_pinned_and_frames_round_trip() {
    let (mut channel, mut child) = connected("echo");
    assert_eq!(
        channel.peer.as_ref().unwrap().credentials.pid as u32,
        child.0.id()
    );
    assert_ne!(child.0.id(), std::process::id());
    for message in [
        vec![],
        b"credential bytes stay in their frame".to_vec(),
        vec![7; MAX_MESSAGE],
    ] {
        channel.send(&message).unwrap();
        assert_eq!(channel.receive().unwrap(), message);
    }
    drop(channel);
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn inherited_endpoint_cannot_substitute_a_descendant_sender() {
    let (mut channel, mut child) = connected("swap");
    channel.send(b"fork").unwrap();
    let error = channel.receive().unwrap_err();
    assert!(error.to_string().contains("sender changed"), "{error}");
    assert!(channel.closed);
    assert!(channel.send(b"again").is_err());
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn buffered_data_from_a_dead_peer_is_refused() {
    let (mut channel, mut child) = connected("dead");
    // Queue the fixture trigger without a post-send liveness check: this
    // test observes receive only after the peer has exited.
    channel
        .stream
        .write_all(&[0, 0, 0, 3, b'd', b'i', b'e'])
        .unwrap();
    assert!(child.0.wait().unwrap().success());
    assert!(channel
        .receive()
        .unwrap_err()
        .to_string()
        .contains("no longer alive"));
    assert!(channel.closed);
}

#[test]
fn oversized_frames_permanently_close_the_channel() {
    let (mut channel, mut child) = connected("oversize");
    channel.send(b"oversize").unwrap();
    assert!(channel
        .receive()
        .unwrap_err()
        .to_string()
        .contains("exceeds"));
    assert!(channel.closed);
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn partial_frame_is_bounded_and_not_reusable() {
    let (mut channel, mut child) = connected("partial");
    channel.send(b"partial").unwrap();
    let started = Instant::now();
    assert!(channel.receive().is_err());
    assert!(started.elapsed() < Duration::from_secs(15));
    assert!(channel.closed);
    assert!(channel.receive().is_err());
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn incorrect_creator_or_sender_uid_is_refused() {
    let wrong = if uid() == 0 { 1 } else { 0 };
    let (left, _right) = UnixStream::pair().unwrap();
    assert!(Channel::connect(left, uid(), wrong).is_err());
    let (left, right) = UnixStream::pair().unwrap();
    let _child = spawn(right, "echo");
    assert!(Channel::connect(left, wrong, uid()).is_err());
}

#[test]
fn absolute_deadline_expires_even_without_a_syscall() {
    let past = Instant::now();
    assert_eq!(remaining(past).unwrap_err().kind(), io::ErrorKind::TimedOut);
}

#[test]
fn trickling_bytes_spend_one_frame_deadline() {
    let (mut channel, mut child) = connected("trickle");
    channel.send(b"trickle").unwrap();
    let started = Instant::now();
    assert!(channel.receive().is_err());
    assert!(started.elapsed() >= Duration::from_secs(4));
    assert!(started.elapsed() < Duration::from_secs(15));
    assert!(channel.closed);
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn oversized_outbound_payload_closes_before_writing_a_frame() {
    let (mut channel, mut child) = connected("echo");
    assert!(channel.send(&vec![0; MAX_MESSAGE + 1]).is_err());
    assert!(channel.closed);
    assert!(channel.send(b"valid now").is_err());
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn malformed_greeting_cannot_become_an_authenticated_channel() {
    let (left, mut right) = UnixStream::pair().unwrap();
    sys::prepare(&right).unwrap();
    right.write_all(b"TDAT000\n").unwrap();
    assert!(Channel::connect(left, uid(), uid())
        .err()
        .unwrap()
        .to_string()
        .contains("greeting"));
}

#[test]
fn creator_refusal_shuts_down_other_copies_of_the_endpoint() {
    let (left, mut right) = UnixStream::pair().unwrap();
    let _retained_stdin = left.try_clone().unwrap();
    let wrong = if uid() == 0 { 1 } else { 0 };
    assert!(Channel::connect(left, uid(), wrong).is_err());
    right
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    assert_eq!(right.read(&mut [0u8]).unwrap(), 0);
}

#[test]
fn pre_greeting_delegation_is_outside_the_trusted_startup_contract() {
    let (mut channel, mut launcher) = connected("delegate-before-greeting");
    assert_ne!(
        channel.peer.as_ref().unwrap().credentials.pid as u32,
        launcher.0.id()
    );
    channel.send(b"the first holder is pinned").unwrap();
    assert_eq!(channel.receive().unwrap(), b"the first holder is pinned");
    drop(channel);
    assert!(launcher.0.wait().unwrap().success());
}

#[test]
fn kernel_pidfds_have_distinct_inode_identity_for_distinct_processes() {
    let (channel, mut child) = connected("echo");
    let (left, mut right) = UnixStream::pair().unwrap();
    sys::prepare(&left).unwrap();
    sys::prepare(&right).unwrap();
    right.write_all(b"X").unwrap();
    let (_, own) = sys::receive(&left, &mut [0u8]).unwrap();
    let own = File::from(own.pidfd).metadata().unwrap();
    let peer = channel.peer.as_ref().unwrap();
    assert_ne!((peer.device, peer.inode), (own.dev(), own.ino()));
    drop(channel);
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn transport_constructor_refuses_overflow_uids_without_waiting_for_a_peer() {
    for overflow in [65534, u32::MAX] {
        let (left, _right) = UnixStream::pair().unwrap();
        let error = Channel::connect(left, overflow, uid()).err().unwrap();
        assert!(error.to_string().contains("unsupported peer uid"));
    }
}

#[test]
fn a_worker_after_greeting_keeps_the_same_process_sender_pin() {
    let (mut channel, mut child) = connected("worker");
    let peer = channel.peer.as_ref().unwrap();
    let identity = (peer.credentials, peer.device, peer.inode);
    for sequence in 0..4u8 {
        channel.send(&[sequence]).unwrap();
        assert_eq!(channel.receive().unwrap(), [sequence]);
        let peer = channel.peer.as_ref().unwrap();
        assert_eq!((peer.credentials, peer.device, peer.inode), identity);
    }
    drop(channel);
    assert!(child.0.wait().unwrap().success());
}
