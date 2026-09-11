#![deny(unsafe_code)]

//! The compositor owns one private endpoint; its worker alone exchanges frames.

#[cfg_attr(not(feature = "target-recipe"), path = "../../td-authd/src/channel.rs")]
#[cfg_attr(feature = "target-recipe", path = "auth/channel.rs")]
mod channel;
#[cfg_attr(not(feature = "target-recipe"), path = "../../td-authd/src/consent.rs")]
#[cfg_attr(feature = "target-recipe", path = "auth/consent.rs")]
#[allow(
    dead_code,
    reason = "immutable trusted-prompt contract; authority consumer follows"
)]
pub(crate) mod consent;
#[cfg_attr(not(feature = "target-recipe"), path = "../../td-authd/src/sys.rs")]
#[cfg_attr(feature = "target-recipe", path = "auth/sys.rs")]
mod sys;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant};

const VERSION: &[u8] = b"TDLA002\n";
const CAPACITY: usize = 16;
const QUEUE_CAPACITY: usize = 1;
const TICK: Duration = Duration::from_millis(250);

enum Work {
    Terminal(Terminal),
    Secret(std::sync::Arc<crate::secret_client::Attempt>),
}

#[derive(Clone)]
pub(crate) struct Launcher {
    send: SyncSender<Work>,
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
        prepare_session(&mut wire)?;
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

    pub fn unlock(
        &self,
        attempt: std::sync::Arc<crate::secret_client::Attempt>,
    ) -> Result<(), String> {
        match self.send.try_send(Work::Secret(attempt)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err("authority request already pending".into()),
            Err(TrySendError::Disconnected(_)) => Err("secret authority unavailable".into()),
        }
    }

    pub fn launch(&self) -> Result<(), String> {
        match self.send.try_send(Work::Terminal(Terminal::Home)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err("terminal launch is already pending".into()),
            Err(TrySendError::Disconnected(_)) => Err("terminal authority is unavailable".into()),
        }
    }

    pub fn launch_task(&self) -> Result<(), String> {
        match self.send.try_send(Work::Terminal(Terminal::Task)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err("terminal launch is already pending".into()),
            Err(TrySendError::Disconnected(_)) => Err("terminal authority is unavailable".into()),
        }
    }
}

#[derive(Clone, Copy)]
enum Terminal {
    Home,
    Task,
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

pub(crate) trait Exchange {
    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, String>;
}

impl Exchange for channel::Channel {
    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, String> {
        self.send(request).map_err(|e| e.to_string())?;
        self.receive().map_err(|e| e.to_string())
    }
}

/// Complete prior-generation cleanup before any device or input admission.
fn prepare_session(wire: &mut impl Exchange) -> Result<(), String> {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(5))
        .ok_or("secret session preparation deadline overflow")?;
    if wire.exchange(&[0x10])? != [0x90] {
        return Err("authority refused secret session preparation".into());
    }
    loop {
        let response = wire.exchange(&[0x11])?;
        if Instant::now() >= deadline {
            return Err("secret session preparation expired".into());
        }
        match response.as_slice() {
            [0x91, 2] => return Ok(()),
            [0x91, 1] => std::thread::sleep(Duration::from_millis(10)),
            _ => return Err("invalid secret session preparation response".into()),
        }
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

    fn start(
        &mut self,
        wire: &mut impl Exchange,
        terminal: Terminal,
    ) -> Result<Option<&'static str>, String> {
        if self.handles.len() >= CAPACITY {
            return Ok(Some("terminal launch limit reached"));
        }
        let request = match terminal {
            Terminal::Home => [1],
            Terminal::Task => [4],
        };
        match wire.exchange(&request)?.as_slice() {
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

fn worker(mut wire: impl Exchange, receive: Receiver<Work>) -> Result<(), String> {
    let mut processes = Processes::new();
    let mut secrets = crate::secret_client::Client::default();
    loop {
        match receive.recv_timeout(TICK) {
            Ok(Work::Secret(attempt)) => secrets.start(&mut wire, attempt)?,
            Ok(Work::Terminal(terminal)) => {
                if let Some(error) = processes.start(&mut wire, terminal)? {
                    let _ = writeln!(std::io::stderr().lock(), "td-compositor: {error}");
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("launcher disconnected".into()),
        }
        if let Some(error) = processes.poll(&mut wire)? {
            let _ = writeln!(std::io::stderr().lock(), "td-compositor: {error}");
        }
        secrets.tick(&mut wire)?;
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
    fn preparation_waits_for_cleanup_and_never_retries_uncertain_requests() {
        let mut pending = wire(vec![vec![0x90], vec![0x91, 1], vec![0x91, 2]]);
        prepare_session(&mut pending).unwrap();
        assert_eq!(pending.requests, [vec![0x10], vec![0x11], vec![0x11]]);
        for replies in [
            vec![vec![0x90, 0]],
            vec![vec![0x90], vec![0x91, 0]],
            vec![vec![0x90], vec![0x91, 3]],
            vec![vec![0x90], vec![0x91, 2, 0]],
            vec![],
        ] {
            let mut refused = wire(replies);
            assert!(prepare_session(&mut refused).is_err());
            assert_eq!(
                refused
                    .requests
                    .iter()
                    .filter(|request| **request == [0x10])
                    .count(),
                1
            );
        }
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
        assert_eq!(p.start(&mut w, Terminal::Home).unwrap(), None);
        assert_eq!(p.start(&mut w, Terminal::Task).unwrap(), None);
        assert_eq!(w.requests[0], [1]);
        assert_eq!(w.requests[1], [4]);
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
            assert!(Processes::new()
                .start(&mut wire(vec![answer]), Terminal::Home)
                .is_err());
        }
        let mut p = Processes::new();
        let mut w = wire(vec![handle(1), vec![0x82, 1], handle(1)]);
        p.start(&mut w, Terminal::Home).unwrap();
        p.poll(&mut w).unwrap();
        assert!(p.start(&mut w, Terminal::Home).is_err());
        let mut p = Processes::new();
        let mut w = wire((1..=16).map(handle).collect());
        for _ in 0..16 {
            assert_eq!(p.start(&mut w, Terminal::Home).unwrap(), None);
        }
        assert!(p.start(&mut w, Terminal::Home).unwrap().is_some());
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
            processes.start(&mut wire, Terminal::Home).unwrap();
            assert!(processes.poll(&mut wire).is_err());
        }
        for status in [vec![], vec![0x83, 0], vec![0x82, 1]] {
            assert!(Processes::new().poll(&mut wire(vec![status])).is_err());
        }
        for response in [vec![0xff, 1], vec![0xff, 2]] {
            let mut processes = Processes::new();
            assert!(processes
                .start(&mut wire(vec![response]), Terminal::Home)
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
        assert!(send.try_send(Work::Terminal(Terminal::Home)).is_ok());
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
        assert!(matches!(receive.try_recv(), Ok(Work::Terminal(Terminal::Home))));
        assert!(launcher.launch_task().is_ok());
        assert!(matches!(receive.try_recv(), Ok(Work::Terminal(Terminal::Task))));
        assert!(receive.try_recv().is_err());
        drop(receive);
        assert!(launcher.launch().is_err());
    }
}
