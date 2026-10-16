#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    ports::{IoProgress, Tick, Transport},
    tls_io::TLS_WIRE_BYTES,
    tls_policy::{tests::relay_body_tests::completed_body, TlsConnection},
};
use std::{cell::RefCell, rc::Rc, time::Duration};
use td_mime::work::Charge;
type Buffer = Box<[u8; TLS_WIRE_BYTES]>;
type Peer = TlsConnection<Buffer>;
#[derive(Default)]
struct Observed {
    checks: usize,
    possible: usize,
    drops: usize,
}
struct Fake {
    clock: Arc<dyn Clock>,
    limit: Instant,
    returned_limit: Option<Instant>,
    observed: Rc<RefCell<Observed>>,
    refuse_check: bool,
    refuse_possible: bool,
}
impl Fake {
    fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            limit: Instant::now() + Duration::from_secs(10),
            returned_limit: None,
            observed: Rc::default(),
            refuse_check: false,
            refuse_possible: false,
        }
    }
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.observed.borrow_mut().drops += 1;
    }
}
impl Authority for Fake {
    type Final = ();
    fn clock(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }
    fn deadline(&self) -> Deadline {
        Deadline::after(Tick(0), 1000).unwrap()
    }
    fn limit(&self) -> Instant {
        self.limit
    }
    fn current(&mut self, _: &mut Meter) -> Result<Instant, dispatch::Error> {
        self.observed.borrow_mut().checks += 1;
        if self.refuse_check {
            Err(dispatch::Error::Admission(ports::Error::Forbidden))
        } else {
            Ok(self.returned_limit.unwrap_or(self.limit))
        }
    }
    fn possible(self, _: &mut Meter) -> Result<(), dispatch::Error> {
        self.observed.borrow_mut().possible += 1;
        if self.refuse_possible {
            Err(dispatch::Error::Admission(ports::Error::Busy))
        } else {
            Ok(())
        }
    }
}
struct Source(Vec<u8>);
impl BlobReader for Source {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<usize, ports::Error> {
        let offset = offset as usize;
        let n = out.len().min(self.0.len() - offset);
        out[..n].copy_from_slice(&self.0[offset..offset + n]);
        Ok(n)
    }
}
struct Cells {
    input: [u8; body::INPUT_BYTES],
    output: [u8; body::OUTPUT_BYTES],
}
impl Cells {
    fn new() -> Self {
        Self {
            input: [0; body::INPUT_BYTES],
            output: [0; body::OUTPUT_BYTES],
        }
    }
    fn scratch(&mut self) -> Scratch<'_> {
        Scratch {
            input: &mut self.input,
            output: &mut self.output,
        }
    }
}
fn meter() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 1000).unwrap(),
        Charge {
            io_bytes: 64 * 1024 * 1024,
            output_bytes: 64 * 1024 * 1024,
            records: 100_000,
            unlinks: 0,
        },
    )
}
fn collect(peer: &mut Peer, bytes: &mut Vec<u8>) {
    for _ in 0..1000 {
        let mut cell = [0; 4096];
        match peer.read(&mut cell) {
            Ok(IoProgress::Bytes(n)) => bytes.extend_from_slice(&cell[..n]),
            _ => break,
        }
    }
}
fn drive(
    mut owner: Engine<'_, Buffer, Fake>,
    peer: &mut Peer,
    bytes: &mut Vec<u8>,
) -> Result<Done<Buffer, ()>, Refusal<Buffer>> {
    let observed = owner.plan.observed.clone();
    for _ in 0..100_000 {
        collect(peer, bytes);
        let before = observed.borrow().checks;
        let writes = owner.exchange.writes_pending();
        let result = owner.advance();
        assert_eq!(observed.borrow().checks, before + usize::from(writes));
        match result? {
            State::Pending(next) => owner = next,
            State::Ready(done) => {
                collect(peer, bytes);
                return Ok(done);
            }
        }
        std::thread::yield_now();
    }
    panic!("native body did not complete");
}
#[test]
fn every_write_and_flush_checks_before_body_and_possible_follows_completion() {
    for count in [0, 1, 500] {
        let (_serial, empty, mut peer, clock) = completed_body();
        let (session, _, _) = empty.into_parts();
        let fake = Fake::new(clock);
        let observed = fake.observed.clone();
        let mut source = Source(b".line\r\n..next\r\n".repeat(count));
        let expected = b"..line\r\n...next\r\n".repeat(count);
        let mut cells = Cells::new();
        let (mut wire, mut db) = (meter(), meter());
        let owner = Engine::new(
            fake,
            session,
            &mut source,
            Body::SevenBit,
            cells.scratch(),
            &mut wire,
            &mut db,
        )
        .ok()
        .unwrap();
        let mut bytes = Vec::new();
        let done = drive(owner, &mut peer, &mut bytes).ok().unwrap();
        for _ in 0..1000 {
            collect(&mut peer, &mut bytes);
            if bytes.len() == expected.len() {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(bytes, expected);
        assert!(!bytes.windows(5).any(|b| b == b"\r\n.\r\n"));
        assert!(observed.borrow().checks >= if count == 0 { 1 } else { 2 });
        assert_eq!(observed.borrow().possible, 1);
        assert_eq!(observed.borrow().drops, 1);
        drop(done.body.into_buffers().unwrap());
        drop(peer.into_buffers().unwrap());
    }
}
#[test]
fn live_refusal_or_shortened_expiry_prevents_all_output_and_possible() {
    for shortened in [false, true] {
        let (_serial, empty, mut peer, clock) = completed_body();
        let (session, _, _) = empty.into_parts();
        let mut fake = Fake::new(clock);
        fake.refuse_check = !shortened;
        if shortened {
            fake.returned_limit = Some(Instant::now());
        }
        let observed = fake.observed.clone();
        let mut source = Source(b"message\r\n".to_vec());
        let mut cells = Cells::new();
        let (mut wire, mut db) = (meter(), meter());
        let owner = Engine::new(
            fake,
            session,
            &mut source,
            Body::SevenBit,
            cells.scratch(),
            &mut wire,
            &mut db,
        )
        .ok()
        .unwrap();
        let mut bytes = Vec::new();
        let failure = drive(owner, &mut peer, &mut bytes).err().unwrap();
        assert_eq!(
            failure.error,
            if shortened {
                Error::Protocol(ports::Error::Deadline)
            } else {
                Error::Publication(dispatch::Error::Admission(ports::Error::Forbidden))
            }
        );
        drop(failure.buffers.unwrap());
        collect(&mut peer, &mut bytes);
        assert!(bytes.is_empty());
        assert_eq!(observed.borrow().checks, 1);
        assert_eq!(observed.borrow().possible, 0);
        assert_eq!(observed.borrow().drops, 1);
        drop(peer.into_buffers().unwrap());
    }
}
#[test]
fn possible_refusal_closes_completed_body_without_a_terminator() {
    let (_serial, empty, mut peer, clock) = completed_body();
    let (session, _, _) = empty.into_parts();
    let mut fake = Fake::new(clock);
    fake.refuse_possible = true;
    let observed = fake.observed.clone();
    let mut source = Source(b".message\r\n".to_vec());
    let mut cells = Cells::new();
    let (mut wire, mut db) = (meter(), meter());
    let owner = Engine::new(
        fake,
        session,
        &mut source,
        Body::SevenBit,
        cells.scratch(),
        &mut wire,
        &mut db,
    )
    .ok()
    .unwrap();
    let mut bytes = Vec::new();
    let failure = drive(owner, &mut peer, &mut bytes).err().unwrap();
    assert_eq!(
        failure.error,
        Error::Publication(dispatch::Error::Admission(ports::Error::Busy))
    );
    drop(failure.buffers.unwrap());
    for _ in 0..1000 {
        collect(&mut peer, &mut bytes);
        if bytes.len() == 11 {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(bytes, b"..message\r\n");
    assert_eq!(observed.borrow().possible, 1);
    assert_eq!(observed.borrow().drops, 1);
    drop(peer.into_buffers().unwrap());
}
#[test]
fn expired_constructor_and_malformed_body_never_publish_possible() {
    for expired in [false, true] {
        let (_serial, empty, mut peer, clock) = completed_body();
        let (session, _, _) = empty.into_parts();
        let mut fake = Fake::new(clock);
        if expired {
            fake.limit = Instant::now();
        }
        let observed = fake.observed.clone();
        let mut source = Source(b"bare\n".to_vec());
        let mut cells = Cells::new();
        let (mut wire, mut db) = (meter(), meter());
        let mut bytes = Vec::new();
        let failure = match Engine::new(
            fake,
            session,
            &mut source,
            Body::SevenBit,
            cells.scratch(),
            &mut wire,
            &mut db,
        ) {
            Ok(owner) => drive(owner, &mut peer, &mut bytes).err().unwrap(),
            Err(error) => error,
        };
        assert_eq!(
            failure.error,
            Error::Protocol(if expired {
                ports::Error::Deadline
            } else {
                ports::Error::Invalid
            })
        );
        drop(failure.buffers.unwrap());
        collect(&mut peer, &mut bytes);
        assert!(bytes.is_empty());
        assert_eq!(observed.borrow().possible, 0);
        assert_eq!(observed.borrow().drops, 1);
        drop(peer.into_buffers().unwrap());
    }
}
