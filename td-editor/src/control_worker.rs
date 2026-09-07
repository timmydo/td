//! Bounded control transport. The worker never owns editor state.

use crate::control::{frame, Decoder, Refusal, Request};
use crate::control_socket::Socket;
use crate::Error;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

pub(crate) const CONNECTIONS: usize = 8;
const IO_BYTES: usize = 16 * 1024;
const DEADLINE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(10);
const IDLE_POLL: Duration = Duration::from_millis(100);

#[derive(Debug)]
struct Live {
    active: AtomicBool,
    deadline: Instant,
}

impl Live {
    fn at(&self, now: Instant) -> bool {
        self.active.load(Ordering::Relaxed) && now < self.deadline
    }
}

/// One request. Dropping it without replying closes its connection.
/// Liveness neither retains a snapshot nor replaces UI target admission.
#[derive(Debug)]
pub struct Job {
    request: Request,
    live: Arc<Live>,
    reply: Option<SyncSender<Vec<u8>>>,
    worker: Thread,
}

impl Job {
    pub fn request(&self) -> &Request {
        &self.request
    }

    pub fn is_live(&self) -> bool {
        self.live.at(Instant::now())
    }

    /// Invoke at most once, only if live immediately before UI admission.
    /// Expiry/disconnect during execution cannot roll back an accepted edit.
    pub fn respond_with(self, dispatch: impl FnOnce(&Request) -> String) -> crate::Result<bool> {
        if !self.is_live() {
            return Ok(false);
        }
        let payload = dispatch(&self.request);
        self.respond(payload.as_bytes())
    }

    /// Copies only a valid bounded payload. Success means queued, not delivered.
    pub fn respond(self, payload: &[u8]) -> crate::Result<bool> {
        if !self.is_live() {
            return Ok(false);
        }
        let response = frame(payload)?;
        Ok(self
            .reply
            .as_ref()
            .is_some_and(|reply| reply.try_send(response).is_ok()))
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // Disconnect before waking: the next step must see an abandoned job.
        self.reply.take();
        self.worker.unpark();
    }
}

#[derive(Debug)]
pub struct Worker {
    requests: Receiver<Job>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<io::Result<()>>>,
}

impl Worker {
    /// Takes ownership of an already published private nonblocking listener.
    pub fn start(socket: Socket) -> io::Result<Self> {
        let (sender, requests) = mpsc::sync_channel(CONNECTIONS);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("td-editor-control".into())
            .spawn(move || run(socket, sender, &stopping))?;
        Ok(Self {
            requests,
            stop,
            thread: Some(thread),
        })
    }

    /// Never waits; makes at most eight receives, returning the first live job.
    /// After eight expired jobs, disconnection is reported on a later call.
    pub fn try_request(&self) -> io::Result<Option<Job>> {
        for _ in 0..CONNECTIONS {
            match self.requests.try_recv() {
                Ok(job) if job.is_live() => return Ok(Some(job)),
                Ok(_) => {}
                Err(TryRecvError::Empty) => return Ok(None),
                Err(TryRecvError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "control worker stopped",
                    ))
                }
            }
        }
        Ok(None)
    }

    pub fn close(mut self) -> io::Result<()> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        thread.thread().unpark();
        thread
            .join()
            .map_err(|_| io::Error::other("control worker failed"))?
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

enum Phase {
    Reading(Decoder),
    Waiting(Receiver<Vec<u8>>),
    Writing { bytes: Vec<u8>, sent: usize },
    Closed,
}

struct Connection {
    stream: UnixStream,
    live: Arc<Live>,
    phase: Phase,
}

impl Connection {
    fn new(stream: UnixStream, now: Instant) -> io::Result<Self> {
        let deadline = now
            .checked_add(DEADLINE)
            .ok_or_else(|| io::Error::other("control deadline overflow"))?;
        Ok(Self {
            stream,
            live: Arc::new(Live {
                active: AtomicBool::new(true),
                deadline,
            }),
            phase: Phase::Reading(Decoder::default()),
        })
    }

    fn step(&mut self, now: Instant, scratch: &mut [u8], requests: &SyncSender<Job>) -> bool {
        if !self.live.at(now) {
            return false;
        }
        match &mut self.phase {
            Phase::Reading(decoder) => match self.stream.read(scratch) {
                Ok(0) => self.refuse(Refusal {
                    id: 0,
                    error: Error::Protocol,
                }),
                Ok(count) => {
                    let Some(bytes) = scratch.get(..count) else {
                        return false;
                    };
                    if let Err(error) = decoder.push(bytes) {
                        return self.refuse(Refusal { id: 0, error });
                    }
                    let Some(payload) = decoder.payload() else {
                        return true;
                    };
                    let request = match Request::parse(payload) {
                        Ok(request) => request,
                        Err(refusal) => return self.refuse(refusal),
                    };
                    let (reply, response) = mpsc::sync_channel(1);
                    let job = Job {
                        request,
                        live: Arc::clone(&self.live),
                        reply: Some(reply),
                        worker: thread::current(),
                    };
                    if requests.try_send(job).is_err() {
                        return false;
                    }
                    self.phase = Phase::Waiting(response);
                    true
                }
                Err(error) => transient(&error),
            },
            Phase::Waiting(response) => match response.try_recv() {
                Ok(bytes) => {
                    self.phase = Phase::Writing { bytes, sent: 0 };
                    self.write()
                }
                Err(TryRecvError::Empty) => true,
                Err(TryRecvError::Disconnected) => false,
            },
            Phase::Writing { .. } => self.write(),
            // A failed refusal frame leaves this defensive terminal state.
            Phase::Closed => false,
        }
    }

    fn write(&mut self) -> bool {
        match &mut self.phase {
            Phase::Writing { bytes, sent } => {
                let end = sent.saturating_add(IO_BYTES).min(bytes.len());
                let Some(part) = bytes.get(*sent..end) else {
                    return false;
                };
                match self.stream.write(part) {
                    Ok(0) => false,
                    Ok(count) => {
                        *sent = sent.saturating_add(count);
                        *sent < bytes.len()
                    }
                    Err(error) => transient(&error),
                }
            }
            _ => false,
        }
    }

    fn refuse(&mut self, refusal: Refusal) -> bool {
        // Drop a partially allocated request before constructing the response.
        self.phase = Phase::Closed;
        match frame(refusal.response().as_bytes()) {
            Ok(bytes) => {
                self.phase = Phase::Writing { bytes, sent: 0 };
                true
            }
            Err(_) => false,
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.live.active.store(false, Ordering::Relaxed);
    }
}

fn transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

fn retry_accept(error: &io::Error) -> bool {
    // Linux: ENOMEM, ENFILE, EMFILE, ECONNABORTED, ENOBUFS. Retry next park.
    transient(error) || matches!(error.raw_os_error(), Some(12 | 23 | 24 | 103 | 105))
}

fn poll_interval(idle: bool) -> Duration {
    if idle {
        IDLE_POLL
    } else {
        POLL
    }
}

fn run(socket: Socket, requests: SyncSender<Job>, stop: &AtomicBool) -> io::Result<()> {
    let mut connections = Vec::with_capacity(CONNECTIONS);
    let mut scratch = [0; IO_BYTES];
    while !stop.load(Ordering::Relaxed) {
        for _ in connections.len()..CONNECTIONS {
            match socket.accept() {
                Ok(stream) => connections.push(Connection::new(stream, Instant::now())?),
                Err(error) if retry_accept(&error) => break,
                Err(error) => return Err(error),
            }
        }
        connections
            .retain_mut(|connection| connection.step(Instant::now(), &mut scratch, &requests));
        thread::park_timeout(poll_interval(connections.is_empty()));
    }
    drop(connections);
    socket.close()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::control::{Operation, MAX_FRAME};
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    fn connection(now: Instant) -> (Connection, UnixStream) {
        let (server, peer) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();
        (Connection::new(server, now).unwrap(), peer)
    }

    fn request(connection: &mut Connection, peer: &mut UnixStream, now: Instant) -> Job {
        let (tx, rx) = mpsc::sync_channel(CONNECTIONS);
        peer.write_all(&frame(b"1\t7\tstate").unwrap()).unwrap();
        assert!(connection.step(now, &mut [0; IO_BYTES], &tx));
        rx.try_recv().unwrap()
    }

    #[test]
    fn bytewise_request_dispatches_once_without_requiring_eof_and_frames_one_reply() {
        let now = Instant::now();
        let (mut connection, mut peer) = connection(now);
        let (tx, rx) = mpsc::sync_channel(CONNECTIONS);
        let input = frame(b"1\t42\tstate").unwrap();
        for (index, byte) in input.iter().enumerate() {
            peer.write_all(&[*byte]).unwrap();
            assert!(connection.step(now, &mut [0; IO_BYTES], &tx));
            if index + 1 < input.len() {
                assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
            }
        }
        let job = rx.try_recv().unwrap();
        assert_eq!(
            job.request(),
            &Request {
                id: 42,
                operation: Operation::State
            }
        );
        peer.write_all(&frame(b"1\t43\tstate").unwrap()).unwrap();
        assert!(connection.step(now, &mut [0; IO_BYTES], &tx));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        assert!(job.respond(b"1\t42\tok\ttest").unwrap());
        assert!(!connection.step(now, &mut [0; IO_BYTES], &tx));
        let expected = frame(b"1\t42\tok\ttest").unwrap();
        let mut output = vec![0; expected.len()];
        peer.read_exact(&mut output).unwrap();
        assert_eq!(output, expected);
        drop(connection);
        // Later input was never dispatched, regardless of unread peer bytes.
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn malformed_oversized_and_truncated_requests_refuse_before_dispatch() {
        let now = Instant::now();
        let mut extra = frame(b"1\t9\tstate").unwrap();
        extra.push(b'x');
        for (bytes, half_close, expected) in [
            (
                vec![0; 4],
                false,
                Refusal {
                    id: 0,
                    error: Error::Protocol,
                },
            ),
            (
                ((MAX_FRAME + 1) as u32).to_be_bytes().to_vec(),
                false,
                Refusal {
                    id: 0,
                    error: Error::Limit,
                },
            ),
            (
                vec![0, 0],
                true,
                Refusal {
                    id: 0,
                    error: Error::Protocol,
                },
            ),
            (
                frame(b"1\t9\tnew\textra").unwrap(),
                false,
                Refusal {
                    id: 9,
                    error: Error::Protocol,
                },
            ),
            (
                extra,
                false,
                Refusal {
                    id: 0,
                    error: Error::Protocol,
                },
            ),
        ] {
            let (mut connection, mut peer) = connection(now);
            let (tx, rx) = mpsc::sync_channel(CONNECTIONS);
            peer.write_all(&bytes).unwrap();
            if half_close {
                peer.shutdown(std::net::Shutdown::Write).unwrap();
            }
            assert!(connection.step(now, &mut [0; IO_BYTES], &tx));
            if half_close {
                assert!(connection.step(now, &mut [0; IO_BYTES], &tx));
            }
            assert!(!connection.step(now, &mut [0; IO_BYTES], &tx));
            assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
            let expected = frame(expected.response().as_bytes()).unwrap();
            let mut output = vec![0; expected.len()];
            peer.read_exact(&mut output).unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn deadline_covers_reading_ui_wait_and_blocked_response_without_renewal() {
        let now = Instant::now();
        let expiry = now + DEADLINE;
        let (tx, _) = mpsc::sync_channel(CONNECTIONS);
        let (mut reading, mut peer) = connection(now);
        peer.write_all(&[0]).unwrap();
        assert!(reading.step(expiry - Duration::from_nanos(1), &mut [0; IO_BYTES], &tx));
        assert!(!reading.step(expiry, &mut [0; IO_BYTES], &tx));

        let (mut waiting, mut peer) = connection(now);
        let job = request(&mut waiting, &mut peer, now);
        assert!(!waiting.step(expiry, &mut [0; IO_BYTES], &tx));
        drop(waiting);
        assert!(!job.is_live());
        assert!(!job.respond(b"reply").unwrap());

        let (mut writing, mut peer) = connection(now);
        let job = request(&mut writing, &mut peer, now);
        assert!(job.respond(&vec![b'x'; MAX_FRAME]).unwrap());
        assert!(writing.step(now, &mut [0; IO_BYTES], &tx));
        // Exercise backpressure if this host's send buffer fills. A host with
        // a larger buffer may finish; neither outcome changes the deadline.
        for _ in 0..128 {
            if !writing.step(now, &mut [0; IO_BYTES], &tx) {
                break;
            }
        }
        assert!(!writing.step(expiry, &mut [0; IO_BYTES], &tx));

        // Expiry before the first output step is independent of buffer size.
        let (mut writing, mut peer) = connection(now);
        let job = request(&mut writing, &mut peer, now);
        assert!(job.respond(b"reply").unwrap());
        assert!(!writing.step(expiry, &mut [0; IO_BYTES], &tx));
        assert_eq!(
            peer.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn full_queue_dropped_jobs_and_oversized_replies_close_without_blocking() {
        let now = Instant::now();
        let (tx, rx) = mpsc::sync_channel(1);
        let (mut first, mut peer) = connection(now);
        peer.write_all(&frame(b"1\t1\tstate").unwrap()).unwrap();
        assert!(first.step(now, &mut [0; IO_BYTES], &tx));
        let (mut second, mut peer) = connection(now);
        peer.write_all(&frame(b"1\t2\tstate").unwrap()).unwrap();
        assert!(!second.step(now, &mut [0; IO_BYTES], &tx));
        drop(rx.try_recv().unwrap());
        assert!(!first.step(now, &mut [0; IO_BYTES], &tx));

        let (mut connection, mut peer) = connection(now);
        let job = request(&mut connection, &mut peer, now);
        assert_eq!(job.respond(&vec![b'x'; MAX_FRAME + 1]), Err(Error::Limit));
        assert!(!connection.step(now, &mut [0; IO_BYTES], &tx));
    }

    #[test]
    fn expired_queued_jobs_are_skipped_and_cannot_queue_a_late_reply() {
        let now = Instant::now();
        let (tx, requests) = mpsc::sync_channel(CONNECTIONS);
        let worker = Worker {
            requests,
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        };
        let mut connections = Vec::new();
        for id in 0..CONNECTIONS {
            let (mut connection, mut peer) = connection(now);
            peer.write_all(&frame(format!("1\t{id}\tstate").as_bytes()).unwrap())
                .unwrap();
            assert!(connection.step(now, &mut [0; IO_BYTES], &tx));
            if id + 1 != CONNECTIONS {
                connection.live.active.store(false, Ordering::Relaxed);
            }
            connections.push((connection, peer));
        }
        let job = worker.try_request().unwrap().unwrap();
        assert_eq!(job.request().id, (CONNECTIONS - 1) as u64);
        assert!(worker.try_request().unwrap().is_none());
        let (mut expired, mut peer) = connection(now);
        let mut job = request(&mut expired, &mut peer, now);
        job.live = Arc::new(Live {
            active: AtomicBool::new(true),
            deadline: now,
        });
        assert!(!job.is_live());
        assert!(!job.respond(b"late").unwrap());
        drop(tx);
        assert_eq!(
            worker.try_request().err().unwrap().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "tdec-worker-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn next_job(worker: &Worker) -> Job {
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(job) = worker.try_request().unwrap() {
                return job;
            }
            assert!(Instant::now() < until, "worker request timeout");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn real_worker_round_trip_shutdown_cancels_jobs_and_removes_owned_endpoint() {
        let dir = Directory::new();
        let path = dir.0.join("control");
        let worker = Worker::start(Socket::bind(&path).unwrap()).unwrap();
        let mut peer = UnixStream::connect(&path).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        peer.write_all(&frame(b"1\t70\tstate").unwrap()).unwrap();
        let job = next_job(&worker);
        assert_eq!(job.request().id, 70);
        assert!(job.respond(b"1\t70\tok\tready").unwrap());
        let mut output = Vec::new();
        peer.read_to_end(&mut output).unwrap();
        assert_eq!(output, frame(b"1\t70\tok\tready").unwrap());

        let mut peer = UnixStream::connect(&path).unwrap();
        peer.write_all(&frame(b"1\t71\tstate").unwrap()).unwrap();
        let job = next_job(&worker);
        worker.close().unwrap();
        assert!(!job.is_live());
        assert!(!job.respond(b"late").unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn real_worker_limits_admitted_connections_while_existing_clients_progress() {
        let dir = Directory::new();
        let path = dir.0.join("control");
        let worker = Worker::start(Socket::bind(&path).unwrap()).unwrap();
        let mut peers = Vec::new();
        let mut jobs = Vec::new();
        for id in 0..CONNECTIONS {
            let mut peer = UnixStream::connect(&path).unwrap();
            peer.write_all(&frame(format!("1\t{id}\tstate").as_bytes()).unwrap())
                .unwrap();
            jobs.push(next_job(&worker));
            peers.push(peer);
        }
        let mut queued = UnixStream::connect(&path).unwrap();
        queued.write_all(&frame(b"1\t99\tstate").unwrap()).unwrap();
        assert!(worker.try_request().unwrap().is_none());
        // Releasing one job frees a slot; another client's request progresses.
        drop(jobs.pop().unwrap());
        let job = next_job(&worker);
        assert_eq!(job.request().id, 99);
        drop(worker);
        assert!(!job.is_live());
        assert!(jobs.iter().all(|job| !job.is_live()));
        assert!(!path.exists());
    }

    #[test]
    fn listener_retry_and_idle_poll_are_bounded_without_hiding_fatal_errors() {
        for errno in [4, 11, 12, 23, 24, 103, 105] {
            assert!(retry_accept(&io::Error::from_raw_os_error(errno)));
        }
        for errno in [9, 13, 22, 88] {
            assert!(!retry_accept(&io::Error::from_raw_os_error(errno)));
        }
        assert_eq!(poll_interval(true), Duration::from_millis(100));
        assert_eq!(poll_interval(false), Duration::from_millis(10));
    }

    #[test]
    fn mutation_dispatch_checks_liveness_before_execution_and_never_replays_later_frames() {
        for cancelled in [false, true] {
            let now = Instant::now();
            let (mut connection, mut peer) = connection(now);
            let (tx, rx) = mpsc::sync_channel(CONNECTIONS);
            let bytes = frame(b"1\t7\tinsert\t1\t0\t0\t0\t61").unwrap();
            for byte in bytes {
                peer.write_all(&[byte]).unwrap();
                assert!(connection.step(now, &mut [0; IO_BYTES], &tx));
            }
            let job = rx.try_recv().unwrap();
            assert!(job.request().is_edit());
            peer.write_all(&frame(b"1\t8\tinsert\t1\t1\t1\t1\t62").unwrap())
                .unwrap();
            assert!(connection.step(now, &mut [0; IO_BYTES], &tx));
            assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
            if cancelled {
                assert!(!connection.step(now + DEADLINE, &mut [0; IO_BYTES], &tx));
                drop(connection);
            }
            let mut ui = crate::ui::Controller::default();
            ui.dispatch(crate::ui::Event::New).unwrap();
            let mut called = false;
            let queued = job
                .respond_with(|request| {
                    called = true;
                    request.execute(&mut ui).unwrap();
                    "1\t7\tok\t".into()
                })
                .unwrap();
            assert_eq!(called, !cancelled);
            assert_eq!(queued, !cancelled);
            assert_eq!(
                ui.editor().document(1).unwrap().text(),
                if cancelled { "" } else { "a" }
            );
        }
    }

    #[test]
    fn expired_held_job_skips_dispatch_without_waiting_for_worker_cleanup() {
        let now = Instant::now();
        let (mut connection, mut peer) = connection(now - DEADLINE);
        // Decode at its injected acceptance time, then hand off at real expiry.
        let job = request(&mut connection, &mut peer, now - DEADLINE);
        let mut called = false;
        assert!(!job
            .respond_with(|_| {
                called = true;
                "reply".into()
            })
            .unwrap());
        assert!(!called);
    }

    #[test]
    fn disconnect_during_dispatch_does_not_claim_a_reply() {
        let now = Instant::now();
        let (mut connection, mut peer) = connection(now);
        let job = request(&mut connection, &mut peer, now);
        let mut called = false;
        assert!(!job
            .respond_with(|_| {
                called = true;
                drop(connection);
                "reply".into()
            })
            .unwrap());
        assert!(called);
    }

    #[test]
    fn text_request_and_large_reply_resume_to_completion_with_a_draining_peer() {
        let now = Instant::now();
        let (mut connection, mut peer) = connection(now);
        let (tx, rx) = mpsc::sync_channel(CONNECTIONS);
        peer.write_all(&frame(b"1\t8\ttext\t1\t0\t0\t4096").unwrap())
            .unwrap();
        assert!(connection.step(now, &mut [0; IO_BYTES], &tx));
        let job = rx.try_recv().unwrap();
        assert!(matches!(job.request().operation, Operation::Text { .. }));
        let payload = vec![b'x'; IO_BYTES * 3 + 5];
        let expected = frame(&payload).unwrap();
        assert!(job.respond(&payload).unwrap());
        let mut output = Vec::new();
        let mut scratch = [0; IO_BYTES];
        let mut pending = true;
        for _ in 0..128 {
            pending = connection.step(now, &mut scratch, &tx);
            loop {
                match peer.read(&mut scratch) {
                    Ok(0) => break,
                    Ok(count) => output.extend_from_slice(&scratch[..count]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("{error}"),
                }
            }
            if !pending {
                break;
            }
        }
        assert!(!pending);
        assert_eq!(output, expected);
    }

    #[test]
    fn explicit_close_reports_thread_failure() {
        let (_sender, requests) = mpsc::sync_channel(CONNECTIONS);
        let worker = Worker {
            requests,
            stop: Arc::new(AtomicBool::new(false)),
            thread: Some(thread::spawn(|| {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            })),
        };
        assert_eq!(
            worker.close().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
