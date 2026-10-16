#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    ids::AttemptId,
    ports::{Deadline, FlushProgress, IoProgress, Tick, Transport},
    tls_io::TLS_WIRE_BYTES,
    tls_policy::tests::relay_body_tests::completed_body,
    tls_policy::TlsConnection,
};
use std::{cell::RefCell, rc::Rc, time::Duration};
use td_mime::work::Charge;
type Buffer = Box<[u8; TLS_WIRE_BYTES]>;
type Peer = TlsConnection<Buffer>;
#[derive(Default)]
struct Observed {
    checks: usize,
    replies: Vec<String>,
}
struct Fake {
    clock: Arc<dyn Clock>,
    limit: Instant,
    observed: Rc<RefCell<Observed>>,
    refuse_check: bool,
    refuse_record: bool,
}
impl Fake {
    fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            limit: Instant::now() + Duration::from_secs(10),
            observed: Rc::default(),
            refuse_check: false,
            refuse_record: false,
        }
    }
}
impl Authority for Fake {
    type Failure = String;
    fn error(_: &String) -> dispatch::Error {
        dispatch::Error::Admission(ports::Error::Busy)
    }
    fn clock(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }
    fn limit(&self) -> Instant {
        self.limit
    }
    fn current(&self, _: &mut Meter) -> Result<Instant, dispatch::Error> {
        self.observed.borrow_mut().checks += 1;
        if self.refuse_check {
            Err(dispatch::Error::Admission(ports::Error::Forbidden))
        } else {
            Ok(self.limit)
        }
    }
    fn record(self, text: &str) -> Result<Outcome, String> {
        self.observed.borrow_mut().replies.push(text.to_owned());
        if self.refuse_record || Instant::now() >= self.limit {
            Err(text.to_owned())
        } else {
            Ok(Outcome {
                attempt: AttemptId::from_bytes([7; 16]),
                recipients: 1,
                old_sequence: 3,
                new_sequence: 4,
            })
        }
    }
}
fn meter() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 1000).unwrap(),
        Charge {
            io_bytes: 64 * 1024 * 1024,
            output_bytes: 8192,
            records: 100_000,
            unlinks: 0,
        },
    )
}
struct Cells {
    input: [u8; 512],
    line: [u8; 512],
    reply: [u8; 4096],
    command: [u8; 3],
}
impl Cells {
    fn new() -> Self {
        Self {
            input: [0; 512],
            line: [0; 512],
            reply: [0; 4096],
            command: [0; 3],
        }
    }
    fn scratch(&mut self) -> Scratch<'_> {
        Scratch {
            input: &mut self.input,
            line: &mut self.line,
            reply: &mut self.reply,
            command: &mut self.command,
        }
    }
}
fn drive(
    mut owner: Engine<'_, Buffer, Fake>,
    peer: &mut Peer,
    response: &[u8],
    chunk: usize,
    eof: bool,
) -> Result<Completed<Buffer>, Failure<Buffer, String>> {
    let mut received = Vec::new();
    let mut sent = 0;
    let mut closed = false;
    for _ in 0..100_000 {
        if !closed {
            let mut buffer = [0; 512];
            if let IoProgress::Bytes(n) = peer.read(&mut buffer).unwrap() {
                received.extend_from_slice(&buffer[..n]);
            }
            assert!(b".\r\n".starts_with(&received));
            if received == b".\r\n" {
                if sent < response.len() {
                    let end = response.len().min(sent + chunk);
                    if let IoProgress::Bytes(n) = peer.write(&response[sent..end]).unwrap() {
                        sent += n;
                    }
                }
                if peer.flush().unwrap() == FlushProgress::Complete && sent == response.len() && eof
                {
                    peer.abort();
                    closed = true;
                }
            }
        }
        match owner.advance()? {
            State::Pending(next) => owner = next,
            State::Ready(done) => {
                assert_eq!(received, b".\r\n");
                return Ok(done);
            }
        }
        std::thread::yield_now();
    }
    panic!("final reply did not finish");
}
#[test]
fn actual_final_reply_is_recorded_before_success_and_failure_retains_it() {
    for chunk in [1, 512] {
        for response in [
            b"250 queued\r\n".as_slice(),
            b"450 try later\r\n",
            b"550 refused\r\n",
        ] {
            for refuse_record in [false, true] {
                let (_serial, body, mut peer, clock) = completed_body();
                let mut authority = Fake::new(clock);
                authority.refuse_record = refuse_record;
                let observed = authority.observed.clone();
                let mut cells = Cells::new();
                let mut wire = meter();
                let mut db = meter();
                let owner = Engine::new(authority, body, cells.scratch(), &mut wire, &mut db)
                    .ok()
                    .unwrap();
                let result = drive(owner, &mut peer, response, chunk, false);
                let text = std::str::from_utf8(&response[..response.len() - 2]).unwrap();
                assert_eq!(observed.borrow().replies, [text]);
                assert!(observed.borrow().checks >= 2);
                match result {
                    Ok(done) => {
                        assert!(!refuse_record);
                        assert_eq!(done.outcome.new_sequence, 4);
                        drop(done.into_buffers().unwrap());
                    }
                    Err(failure) => {
                        assert!(refuse_record);
                        assert_eq!(failure.retained.as_deref(), Some(text));
                        drop(failure.buffers.unwrap());
                    }
                }
                drop(peer.into_buffers().unwrap());
            }
        }
    }
}
#[test]
fn live_refusal_and_original_expiry_prevent_terminator_output() {
    for expired in [false, true] {
        let (_serial, body, mut peer, clock) = completed_body();
        let mut authority = Fake::new(clock);
        authority.refuse_check = !expired;
        if expired {
            authority.limit = Instant::now();
        }
        let observed = authority.observed.clone();
        let mut cells = Cells::new();
        let mut wire = meter();
        let mut db = meter();
        let failure = match Engine::new(authority, body, cells.scratch(), &mut wire, &mut db) {
            Err(e) => e,
            Ok(owner) => owner.advance().err().unwrap(),
        };
        assert!(failure.retained.is_none());
        assert!(observed.borrow().replies.is_empty());
        assert_eq!(observed.borrow().checks, usize::from(!expired));
        let mut bytes = [0; 512];
        assert!(!matches!(peer.read(&mut bytes), Ok(IoProgress::Bytes(n)) if n>0));
        drop(failure.buffers.unwrap());
        drop(peer.into_buffers().unwrap());
    }
}
#[test]
fn nonfinal_malformed_and_truncated_replies_never_publish() {
    for (response, eof) in [
        (b"354 continue\r\n".as_slice(), false),
        (b"250 bad\n", false),
        (b"250 incomplete", true),
    ] {
        let (_serial, body, mut peer, clock) = completed_body();
        let authority = Fake::new(clock);
        let observed = authority.observed.clone();
        let mut cells = Cells::new();
        let mut wire = meter();
        let mut db = meter();
        let owner = Engine::new(authority, body, cells.scratch(), &mut wire, &mut db)
            .ok()
            .unwrap();
        let failure = drive(owner, &mut peer, response, 1, eof).err().unwrap();
        assert!(failure.retained.is_none());
        assert!(observed.borrow().replies.is_empty());
        drop(failure.buffers.unwrap());
        drop(peer.into_buffers().unwrap());
    }
}

struct OtherClock(Arc<dyn Clock>);
impl Clock for OtherClock {
    fn sample(&self) -> Result<ports::Time, ports::Error> {
        self.0.sample()
    }
}
#[test]
fn another_clock_and_empty_wire_budget_cannot_open_handoff() {
    for wrong_clock in [false, true] {
        let (_serial, body, mut peer, clock) = completed_body();
        let authority = Fake::new(if wrong_clock {
            Arc::new(OtherClock(clock))
        } else {
            clock
        });
        let observed = authority.observed.clone();
        let mut cells = Cells::new();
        let mut wire = if wrong_clock {
            meter()
        } else {
            Meter::new(Deadline::after(Tick(0), 1000).unwrap(), Charge::default())
        };
        let mut db = meter();
        let failure = Engine::new(authority, body, cells.scratch(), &mut wire, &mut db)
            .err()
            .unwrap();
        assert_eq!(
            failure.error,
            Error::Protocol(if wrong_clock {
                ports::Error::Invalid
            } else {
                ports::Error::Capacity
            })
        );
        assert!(failure.retained.is_none());
        assert_eq!(observed.borrow().checks, 0);
        let mut bytes = [0; 512];
        assert!(!matches!(peer.read(&mut bytes), Ok(IoProgress::Bytes(n)) if n>0));
        drop(failure.buffers.unwrap());
        drop(peer.into_buffers().unwrap());
    }
}

#[test]
fn parsed_final_reply_keeps_custody_after_publication_deadline() {
    let (_serial, body, mut peer, clock) = completed_body();
    let authority = Fake::new(clock);
    let observed = authority.observed.clone();
    let mut cells = Cells::new();
    let mut wire = meter();
    let mut db = meter();
    let owner = Engine::new(authority, body, cells.scratch(), &mut wire, &mut db)
        .ok()
        .unwrap();
    let Engine {
        mut result,
        mut command,
        ..
    } = owner;
    let mut received = Vec::new();
    let response = b"250 queued\r\n";
    let mut sent = 0;
    let mut turns = 0;
    let reply = 'read: loop {
        turns += 1;
        assert!(turns <= 100_000);
        let mut bytes = [0; 512];
        if let IoProgress::Bytes(n) = peer.read(&mut bytes).unwrap() {
            received.extend_from_slice(&bytes[..n]);
        }
        assert!(b".\r\n".starts_with(&received));
        if received == b".\r\n" {
            if sent < response.len() {
                if let IoProgress::Bytes(n) = peer.write(&response[sent..]).unwrap() {
                    sent += n;
                }
            }
            let _ = peer.flush().unwrap();
        }
        match command.advance().ok().unwrap() {
            command::Progress::Pending(next) => command = next,
            command::Progress::Ready(reply) => break 'read reply,
        }
        std::thread::yield_now();
    };
    assert_eq!(reply.text().unwrap(), "250 queued");
    // Deterministically simulate descheduling after complete parsing. Native
    // FinalResult.record refuses expired work while retaining this evidence.
    result.limit = Instant::now();
    let failure = Engine::finish(result, reply).err().unwrap();
    assert_eq!(failure.retained.as_deref(), Some("250 queued"));
    assert_eq!(observed.borrow().replies, ["250 queued"]);
    drop(failure.buffers.unwrap());
    drop(peer.into_buffers().unwrap());
}
