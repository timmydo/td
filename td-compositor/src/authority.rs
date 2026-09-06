#![deny(unsafe_code)]

//! The compositor owns one private endpoint; its worker alone exchanges frames.

#[cfg_attr(not(feature = "target-recipe"), path = "../../td-authd/src/channel.rs")]
#[cfg_attr(feature = "target-recipe", path = "auth/channel.rs")]
mod channel;
#[cfg_attr(not(feature = "target-recipe"), path = "../../td-authd/src/sys.rs")]
#[cfg_attr(feature = "target-recipe", path = "auth/sys.rs")]
mod sys;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::Duration;

const VERSION: &[u8] = b"TDLA001\n";
const CAPACITY: usize = 16;
const QUEUE_CAPACITY: usize = 1;
const TICK: Duration = Duration::from_millis(250);

pub(crate) struct Launcher {
    send: SyncSender<()>,
}

impl Launcher {
    /// Must precede every compositor thread, child, and descriptor delegation.
    pub fn connect() -> Result<Self, String> {
        startup()?;
        let mut wire = channel::Channel::from_stdin(0).map_err(|e| e.to_string())?;
        if wire.receive().map_err(|e| e.to_string())? != VERSION {
            return Err("unsupported terminal authority protocol".into());
        }
        wire.send(VERSION).map_err(|e| e.to_string())?;
        if wire.receive().map_err(|e| e.to_string())? != [0x80] {
            return Err("terminal authority refused session admission".into());
        }
        let (send, receive) = mpsc::sync_channel(QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("terminal-authority".into())
            .spawn(move || {
                if let Err(error) = worker(wire, receive) {
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "td-compositor: terminal authority: {error}"
                    );
                }
                // Losing either peer must end this paired service generation.
                std::process::exit(1);
            })
            .map_err(|e| format!("start terminal authority worker: {e}"))?;
        Ok(Self { send })
    }

    pub fn launch(&self) -> Result<(), String> {
        match self.send.try_send(()) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(())) => Err("terminal launch is already pending".into()),
            Err(TrySendError::Disconnected(())) => Err("terminal authority is unavailable".into()),
        }
    }
}

fn startup() -> Result<(), String> {
    let mut status = String::new();
    File::open("/proc/self/status")
        .map_err(|e| e.to_string())?
        .take(8193)
        .read_to_string(&mut status)
        .map_err(|e| e.to_string())?;
    check_status(&status)?;
    audit_descriptors(
        |fd| {
            std::fs::metadata(format!("/proc/self/fd/{fd}"))
                .map(|metadata| (metadata.dev(), metadata.ino()))
                .map_err(|e| format!("missing authority standard descriptor {fd}: {e}"))
        },
        || {
            std::fs::read_dir("/proc/self/fd")
                .map_err(|e| e.to_string())?
                .take(5)
                .map(|entry| {
                    entry
                        .map_err(|e| e.to_string())?
                        .file_name()
                        .to_str()
                        .ok_or("invalid descriptor name")?
                        .parse::<u32>()
                        .map_err(|_| "invalid descriptor number".into())
                })
                .collect()
        },
    )
}

fn audit_descriptors(
    mut identity: impl FnMut(u32) -> Result<(u64, u64), String>,
    enumerate: impl FnOnce() -> Result<Vec<u32>, String>,
) -> Result<(), String> {
    // Check before read_dir can occupy a closed standard descriptor.
    let channel = identity(0)?;
    for fd in [1, 2] {
        if identity(fd)? == channel {
            return Err("compositor log aliases its private authority endpoint".into());
        }
    }
    let mut descriptors = enumerate()?;
    descriptors.sort_unstable();
    if descriptors != [0, 1, 2, 3] {
        return Err("compositor requires only standard authority descriptors".into());
    }
    Ok(())
}

fn check_status(status: &str) -> Result<(), String> {
    let columns = |key: &str| {
        status.lines().find_map(|line| {
            line.strip_prefix(key)
                .map(|value| value.split_whitespace().collect::<Vec<_>>())
        })
    };
    let uid = columns("Uid:").ok_or("missing compositor identity")?;
    let [first, second, third, fourth] = uid.as_slice() else {
        return Err("invalid compositor identity".into());
    };
    let number = first.parse::<u32>().map_err(|_| "invalid compositor uid")?;
    if status.len() > 8192
        || !(1..=999).contains(&number)
        || number.to_string() != *first
        || first != second
        || first != third
        || first != fourth
        || columns("Gid:").as_deref() != Some(uid.as_slice())
        || columns("Threads:").as_deref() != Some(["1"].as_slice())
    {
        return Err("authority compositor requires one dedicated service thread".into());
    }
    Ok(())
}

trait Exchange {
    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, String>;
}

impl Exchange for channel::Channel {
    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, String> {
        self.send(request).map_err(|e| e.to_string())?;
        self.receive().map_err(|e| e.to_string())
    }
}

struct Processes {
    handles: VecDeque<u64>,
    latest: u64,
}

impl Processes {
    fn new() -> Self {
        Self {
            handles: VecDeque::with_capacity(CAPACITY),
            latest: 0,
        }
    }

    fn start(&mut self, wire: &mut impl Exchange) -> Result<Option<&'static str>, String> {
        if self.handles.len() >= CAPACITY {
            return Ok(Some("terminal launch limit reached"));
        }
        match wire.exchange(&[1])?.as_slice() {
            [0xff, 1] => Ok(Some("terminal authority process table is full")),
            [0xff, 2] => Ok(Some("terminal authority could not start the helper")),
            [0x81, handle @ ..] if handle.len() == 8 => {
                let handle = u64::from_be_bytes(handle.try_into().map_err(|_| "invalid handle")?);
                if handle <= self.latest {
                    return Err("terminal authority reused a process handle".into());
                }
                self.latest = handle;
                self.handles.push_back(handle);
                Ok(None)
            }
            _ => Err("invalid terminal authority start response".into()),
        }
    }

    fn poll(&mut self, wire: &mut impl Exchange) -> Result<Option<&'static str>, String> {
        let Some(handle) = self.handles.pop_front() else {
            if wire.exchange(&[3])? != [0x83] {
                return Err("invalid terminal authority heartbeat".into());
            }
            return Ok(None);
        };
        let mut request = [0u8; 9];
        if let Some(first) = request.first_mut() {
            *first = 2;
        }
        request
            .get_mut(1..)
            .ok_or("invalid poll buffer")?
            .copy_from_slice(&handle.to_be_bytes());
        match wire.exchange(&request)?.as_slice() {
            [0x82, 0] => {
                self.handles.push_back(handle);
                Ok(None)
            }
            [0x82, 1] => Ok(None),
            [0x82, 2] => Ok(Some("launched terminal failed")),
            _ => Err("invalid terminal authority process status".into()),
        }
    }
}

fn worker(mut wire: impl Exchange, receive: Receiver<()>) -> Result<(), String> {
    let mut processes = Processes::new();
    loop {
        match receive.recv_timeout(TICK) {
            Ok(()) => {
                if let Some(error) = processes.start(&mut wire)? {
                    let _ = writeln!(std::io::stderr().lock(), "td-compositor: {error}");
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("launcher disconnected".into()),
        }
        if let Some(error) = processes.poll(&mut wire)? {
            let _ = writeln!(std::io::stderr().lock(), "td-compositor: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    struct Wire {
        requests: Vec<Vec<u8>>,
        answers: VecDeque<Vec<u8>>,
    }
    impl Exchange for Wire {
        fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, String> {
            self.requests.push(request.to_vec());
            self.answers.pop_front().ok_or("unexpected request".into())
        }
    }
    fn wire(answers: Vec<Vec<u8>>) -> Wire {
        Wire {
            requests: Vec::new(),
            answers: answers.into(),
        }
    }
    fn handle(number: u64) -> Vec<u8> {
        let mut bytes = vec![0x81];
        bytes.extend_from_slice(&number.to_be_bytes());
        bytes
    }

    #[test]
    fn startup_refuses_human_root_partial_and_multithreaded_identities() {
        let status = "Uid:\t993\t993\t993\t993\nGid:\t993\t993\t993\t993\nThreads:\t1\n";
        assert!(check_status(status).is_ok());
        for invalid in [
            status.replace("993", "1000"),
            status.replace("993", "0"),
            status.replace("Threads:\t1", "Threads:\t2"),
            status.replacen("993", "992", 1),
            status.replace("Gid:", "Absent:"),
            format!("{status}{}", "x".repeat(8192)),
        ] {
            assert!(check_status(&invalid).is_err());
        }
    }

    #[test]
    fn descriptor_admission_checks_holes_and_aliases_before_enumeration() {
        for missing in [0, 1, 2] {
            let enumerated = std::cell::Cell::new(false);
            assert!(audit_descriptors(
                |fd| if fd == missing {
                    Err("closed".into())
                } else {
                    Ok((1, fd as u64))
                },
                || {
                    enumerated.set(true);
                    Ok(vec![0, 1, 2, 3])
                },
            )
            .is_err());
            assert!(!enumerated.get());
        }
        for alias in [1, 2] {
            assert!(audit_descriptors(
                |fd| Ok((1, if fd == alias { 0 } else { fd as u64 })),
                || panic!("alias must fail before enumeration"),
            )
            .is_err());
        }
        assert!(audit_descriptors(|fd| Ok((1, fd as u64)), || Ok(vec![3, 2, 0, 1])).is_ok());
        for descriptors in [vec![0, 1, 2], vec![0, 1, 2, 4], vec![0, 1, 2, 3, 4]] {
            assert!(audit_descriptors(|fd| Ok((1, fd as u64)), || Ok(descriptors)).is_err());
        }
    }

    #[test]
    fn polls_rotate_live_handles_and_retire_each_completion_once() {
        let mut p = Processes::new();
        let mut w = wire(vec![
            handle(1),
            handle(3),
            vec![0x82, 0],
            vec![0x82, 2],
            vec![0x82, 1],
            vec![0x83],
        ]);
        assert_eq!(p.start(&mut w).unwrap(), None);
        assert_eq!(p.start(&mut w).unwrap(), None);
        assert_eq!(p.poll(&mut w).unwrap(), None);
        assert_eq!(p.poll(&mut w).unwrap(), Some("launched terminal failed"));
        assert_eq!(p.poll(&mut w).unwrap(), None);
        assert_eq!(p.poll(&mut w).unwrap(), None);
        assert_eq!(w.requests[2], [2, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(w.requests[3], [2, 0, 0, 0, 0, 0, 0, 0, 3]);
        assert_eq!(w.requests[4], w.requests[2]);
        assert_eq!(w.requests[5], [3]);
    }

    #[test]
    fn invalid_and_reused_handles_are_fatal_but_capacity_is_bounded() {
        for answer in [
            handle(0),
            vec![0x81],
            vec![0x81, 1],
            vec![0xff, 3],
            vec![0xff, 1, 0],
        ] {
            assert!(Processes::new().start(&mut wire(vec![answer])).is_err());
        }
        let mut p = Processes::new();
        let mut w = wire(vec![handle(1), vec![0x82, 1], handle(1)]);
        p.start(&mut w).unwrap();
        p.poll(&mut w).unwrap();
        assert!(p.start(&mut w).is_err());
        let mut p = Processes::new();
        let mut w = wire((1..=16).map(handle).collect());
        for _ in 0..16 {
            assert_eq!(p.start(&mut w).unwrap(), None);
        }
        assert!(p.start(&mut w).unwrap().is_some());
        assert_eq!(w.requests.len(), 16);
    }

    #[test]
    fn malformed_status_and_heartbeat_answers_are_fatal() {
        for status in [
            vec![],
            vec![0x82],
            vec![0x82, 3],
            vec![0x82, 0, 0],
            vec![0xff, 1],
        ] {
            let mut processes = Processes::new();
            let mut wire = wire(vec![handle(1), status]);
            processes.start(&mut wire).unwrap();
            assert!(processes.poll(&mut wire).is_err());
        }
        for status in [vec![], vec![0x83, 0], vec![0x82, 1]] {
            assert!(Processes::new().poll(&mut wire(vec![status])).is_err());
        }
        for response in [vec![0xff, 1], vec![0xff, 2]] {
            let mut processes = Processes::new();
            assert!(processes
                .start(&mut wire(vec![response]))
                .unwrap()
                .is_some());
            assert!(processes.handles.is_empty());
        }
    }

    #[test]
    fn worker_never_retries_a_request_after_uncertain_delivery() {
        struct Broken(std::rc::Rc<std::cell::Cell<usize>>);
        impl Exchange for Broken {
            fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, String> {
                assert_eq!(request, [1]);
                self.0.set(self.0.get() + 1);
                Err("response lost after delivery".into())
            }
        }
        let (send, receive) = mpsc::sync_channel(QUEUE_CAPACITY);
        send.try_send(()).unwrap();
        drop(send);
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        assert!(worker(Broken(calls.clone()), receive).is_err());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn launcher_queue_never_blocks_input_or_retries_a_delivered_request() {
        let (send, receive) = mpsc::sync_channel(QUEUE_CAPACITY);
        let launcher = Launcher { send };
        assert!(launcher.launch().is_ok());
        assert!(launcher.launch().is_err());
        assert_eq!(receive.try_recv(), Ok(()));
        assert!(receive.try_recv().is_err());
        drop(receive);
        assert!(launcher.launch().is_err());
    }
}
