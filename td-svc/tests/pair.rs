//! Exercise the actual pair-run applet, including fd inheritance and teardown.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn fixture(name: &str) -> Vec<String> {
    vec![
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        "--exact".into(),
        name.into(),
        "--nocapture".into(),
        "--test-threads=1".into(),
    ]
}

fn command(left: Vec<String>, right: Vec<String>) -> Command {
    let binary = option_env!("TD_SVC_TEST_BINARY")
        .or(option_env!("CARGO_BIN_EXE_td-svc"))
        .expect("test must name the td-svc binary it exercises");
    let mut command = Command::new(binary);
    command
        .arg("pair-run")
        .arg(left.len().to_string())
        .args(left)
        .args(right);
    command.env("TD_SVC_PAIR_FIXTURE", "1");
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn endpoint() -> Option<UnixStream> {
    std::env::var_os("TD_SVC_PAIR_FIXTURE")?;
    let sockets: std::collections::BTreeSet<_> = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter_map(|p| std::fs::read_link(p).ok())
        .filter(|p| p.to_string_lossy().starts_with("socket:["))
        .collect();
    assert_eq!(
        sockets.len(),
        1,
        "inherited an unintended socket endpoint: {sockets:?}"
    );
    let socket = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    Some(socket)
}

#[test]
fn first_peer() {
    let Some(mut socket) = endpoint() else {
        return;
    };
    socket.write_all(b"first").unwrap();
    let mut reply = [0; 6];
    socket.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"second");
    println!("PAIR-FIRST-{}", std::process::id());
    socket.write_all(b"ack").unwrap();
    // The other peer exits while this one stays alive: the coordinator must kill it.
    std::thread::sleep(Duration::from_secs(30));
    panic!("coordinator left the first peer alive");
}

#[test]
fn second_peer() {
    let Some(mut socket) = endpoint() else {
        return;
    };
    let mut request = [0; 5];
    socket.read_exact(&mut request).unwrap();
    assert_eq!(&request, b"first");
    socket.write_all(b"second").unwrap();
    let mut ack = [0; 3];
    socket.read_exact(&mut ack).unwrap();
    assert_eq!(&ack, b"ack");
    println!("PAIR-SECOND-{}", std::process::id());
}

fn wait(mut child: std::process::Child) -> std::process::Output {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("pair coordinator hung: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn private_channel_connects_only_the_pair_and_peer_exit_ends_both() {
    check_exchange("first_peer", "second_peer", "second");
    check_exchange("second_peer", "first_peer", "first");
}

fn check_exchange(left: &str, right: &str, exiting: &str) {
    let mut child = command(fixture(left), fixture(right)).spawn().unwrap();
    child.stdin.take().unwrap().write_all(&[1]).unwrap();
    let output = wait(child);
    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(&format!("{exiting} paired daemon exited (exit status: 0)")),
        "{stderr}\n{stdout}"
    );
    for prefix in ["PAIR-FIRST-", "PAIR-SECOND-"] {
        let (_, rest) = stdout.split_once(prefix).unwrap();
        let pid: u32 = rest.lines().next().unwrap().parse().unwrap();
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "peer {pid} leaked"
        );
    }
}

#[test]
fn no_grant_and_malformed_grants_launch_neither_peer() {
    for grant in [vec![], vec![0], vec![1, 0]] {
        let mut child = command(fixture("first_peer"), fixture("second_peer"))
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&grant).unwrap();
        let output = wait(child);
        assert!(!output.status.success());
        assert!(
            output.stdout.is_empty(),
            "a peer started without a valid grant"
        );
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains("pair start gate refused"));
    }
}

#[test]
fn an_unreleased_start_gate_waits_without_starting_children() {
    for granted in [false, true] {
        let mut child = command(fixture("first_peer"), fixture("second_peer"))
            .spawn()
            .unwrap();
        if granted {
            child.stdin.as_mut().unwrap().write_all(&[1]).unwrap();
        }
        std::thread::sleep(Duration::from_millis(100));
        assert!(child.try_wait().unwrap().is_none());
        for entry in std::fs::read_dir("/proc").unwrap() {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
                continue;
            }
            let stat = match std::fs::read_to_string(entry.path().join("stat")) {
                Ok(stat) => stat,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => panic!("could not inspect process: {e}"),
            };
            let (_, fields) = stat.rsplit_once(") ").unwrap();
            let ppid: u32 = fields.split_whitespace().nth(1).unwrap().parse().unwrap();
            assert_ne!(
                ppid,
                child.id(),
                "coordinator forked before placement: {stat}"
            );
        }
        drop(child.stdin.take());
        let output = wait(child);
        assert!(!output.status.success());
        assert_eq!(output.stdout.is_empty(), !granted);
    }
}

#[test]
fn second_spawn_failure_reaps_the_first() {
    let mut child = command(
        fixture("first_peer"),
        vec!["/nonexistent/td-pair-peer".into()],
    )
    .spawn()
    .unwrap();
    child.stdin.take().unwrap().write_all(&[1]).unwrap();
    let output = wait(child);
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("paired executable /nonexistent/td-pair-peer"));
}
