#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    clock::TlsClockSource,
    ports::{Tick, Time},
};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        Mutex,
    },
};
use td_crypto::{
    ClientConfig, ClockHandle, Crypto, IdentitySelection, P256Key, PeerEvidence, Provider,
    ServerConfig, ServerIdentity, TlsProtocol, TrustStore, P256_PKCS8_CAPACITY,
};

pub(crate) struct TestClock {
    utc: AtomicI64,
    tick: AtomicU64,
    fail: AtomicBool,
}
impl TestClock {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            utc: AtomicI64::new(1_800_000_000_000),
            tick: AtomicU64::new(0),
            fail: AtomicBool::new(false),
        })
    }
}
impl Clock for TestClock {
    fn sample(&self) -> Result<Time, Error> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(Error::Busy);
        }
        Ok(Time {
            utc_ms: self.utc.load(Ordering::SeqCst),
            monotonic: Tick(self.tick.load(Ordering::SeqCst)),
        })
    }
}

fn der(tag: u8, data: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    if data.len() < 128 {
        out.push(data.len() as u8);
    } else {
        let length = data.len().to_be_bytes();
        let length = length
            .iter()
            .skip_while(|byte| **byte == 0)
            .copied()
            .collect::<Vec<_>>();
        out.push(0x80 | length.len() as u8);
        out.extend_from_slice(&length);
    }
    out.extend_from_slice(data);
    out
}
fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
    der(0x30, &parts.concat())
}
fn oid(bytes: &[u8]) -> Vec<u8> {
    der(6, bytes)
}
fn name(bytes: &[u8]) -> Vec<u8> {
    seq(&[der(0x31, &seq(&[oid(&[0x55, 4, 3]), der(0x0c, bytes)]))])
}
fn extension(id: u8, critical: bool, body: Vec<u8>) -> Vec<u8> {
    let mut parts = vec![oid(&[0x55, 0x1d, id])];
    if critical {
        parts.push(der(1, &[0xff]));
    }
    parts.push(der(4, &body));
    seq(&parts)
}
fn integer(bytes: &[u8]) -> Vec<u8> {
    let mut value = bytes
        .iter()
        .skip_while(|byte| **byte == 0)
        .copied()
        .collect::<Vec<_>>();
    if value.is_empty() || value[0] & 0x80 != 0 {
        value.insert(0, 0);
    }
    der(2, &value)
}
pub(crate) fn certificate(key: &P256Key, signer: &P256Key, ca: bool) -> Vec<u8> {
    certificate_with_serial(key, signer, ca, if ca { 1 } else { 2 })
}
pub(crate) fn certificate_with_serial(
    key: &P256Key,
    signer: &P256Key,
    ca: bool,
    serial: u8,
) -> Vec<u8> {
    let provider = Provider;
    let mut public = [0; 65];
    provider.p256_public(key, &mut public).unwrap();
    let spki = seq(&[
        seq(&[
            oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 2, 1]),
            oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 3, 1, 7]),
        ]),
        der(3, &[&[0][..], &public].concat()),
    ]);
    let algorithm = seq(&[oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 4, 3, 2])]);
    let mut extensions = vec![
        extension(
            0x13,
            true,
            if ca {
                seq(&[der(1, &[0xff])])
            } else {
                seq(&[])
            },
        ),
        extension(0x0f, true, der(3, if ca { &[1, 6] } else { &[7, 0x80] })),
    ];
    if !ca {
        extensions.push(extension(0x11, false, seq(&[der(0x82, b"localhost")])));
        extensions.push(extension(
            0x25,
            false,
            seq(&[oid(&[0x2b, 6, 1, 5, 5, 7, 3, 1])]),
        ));
    }
    let body = seq(&[
        der(0xa0, &der(2, &[2])),
        der(2, &[serial]),
        algorithm.clone(),
        name(b"local-test-root"),
        seq(&[der(0x17, b"250101000000Z"), der(0x17, b"350101000000Z")]),
        name(if ca { b"local-test-root" } else { b"localhost" }),
        spki,
        der(0xa3, &seq(&extensions)),
    ]);
    let mut signature = [0; 64];
    provider.sign_es256(signer, &body, &mut signature).unwrap();
    let signature = seq(&[integer(&signature[..32]), integer(&signature[32..])]);
    seq(&[body, algorithm, der(3, &[&[0][..], &signature].concat())])
}
pub(crate) fn pem(label: &str, bytes: &[u8]) -> Vec<u8> {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = format!("-----BEGIN {label}-----\n").into_bytes();
    let mut count = 0;
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        for (index, shift) in [18, 12, 6, 0].iter().enumerate() {
            output.push(if index > chunk.len() {
                b'='
            } else {
                alphabet[((value >> shift) & 63) as usize]
            });
            count += 1;
            if count % 64 == 0 {
                output.push(b'\n');
            }
        }
    }
    if count % 64 != 0 {
        output.push(b'\n');
    }
    output.extend_from_slice(format!("-----END {label}-----\n").as_bytes());
    output
}
pub(crate) fn identity_material() -> (Arc<ServerIdentity>, Vec<u8>) {
    let provider = Provider;
    let mut raw = [0; P256_PKCS8_CAPACITY];
    let count = provider.generate_p256(&mut raw).unwrap();
    let root = provider.load_p256(&raw[..count]).unwrap();
    let ca = pem("CERTIFICATE", &certificate(&root, &root, true));
    let count = provider.generate_p256(&mut raw).unwrap();
    let key = provider.load_p256(&raw[..count]).unwrap();
    let leaf = pem("CERTIFICATE", &certificate(&key, &root, false));
    let identity = Arc::new(
        ServerIdentity::from_pem(
            &[leaf, ca.clone()].concat(),
            &pem("PRIVATE KEY", &raw[..count]),
            &["localhost"],
            Some(1_800_000_000),
        )
        .unwrap(),
    );
    (identity, ca)
}

fn configs(clock: &Arc<TestClock>) -> (Arc<ClientConfig>, Arc<ServerConfig>) {
    let (identity, ca) = identity_material();
    let roots = TrustStore::from_pem(&ca).unwrap();
    let clock = Arc::new(ClockHandle::new(TlsClockSource::new(clock.clone())));
    (
        Arc::new(ClientConfig::new(&roots, clock.clone(), TlsProtocol::Smtp).unwrap()),
        Arc::new(
            ServerConfig::new(
                &[identity],
                TlsProtocol::Smtp,
                IdentitySelection::DefaultIdentity,
                None,
                clock,
            )
            .unwrap(),
        ),
    )
}

struct Link {
    data: [VecDeque<u8>; 2],
    closed: [bool; 2],
    aborted: [bool; 2],
    calls: [usize; 2],
    reads: [usize; 2],
}
struct Peer {
    link: Arc<Mutex<Link>>,
    side: usize,
}
fn peers() -> (Peer, Peer, Arc<Mutex<Link>>) {
    let link = Arc::new(Mutex::new(Link {
        data: std::array::from_fn(|_| VecDeque::with_capacity(7)),
        closed: [false; 2],
        aborted: [false; 2],
        calls: [0; 2],
        reads: [0; 2],
    }));
    (
        Peer {
            link: link.clone(),
            side: 0,
        },
        Peer {
            link: link.clone(),
            side: 1,
        },
        link,
    )
}
impl Transport for Peer {
    fn read(&mut self, output: &mut [u8]) -> Result<IoProgress, Error> {
        if output.is_empty() {
            return Ok(IoProgress::Pending);
        }
        let mut link = self.link.lock().unwrap();
        link.reads[self.side] += 1;
        if link.aborted[self.side] {
            return Err(Error::Invalid);
        }
        let peer = 1 - self.side;
        if let Some(byte) = link.data[self.side].pop_front() {
            output[0] = byte;
            Ok(IoProgress::Bytes(1))
        } else if link.closed[peer] {
            Ok(IoProgress::Closed)
        } else {
            Ok(IoProgress::Pending)
        }
    }
    fn write(&mut self, input: &[u8]) -> Result<IoProgress, Error> {
        if input.is_empty() {
            return Ok(IoProgress::Pending);
        }
        let mut link = self.link.lock().unwrap();
        let peer = 1 - self.side;
        if link.closed[self.side] || link.aborted[peer] {
            return Err(Error::Invalid);
        }
        link.calls[self.side] += 1;
        if link.calls[self.side].is_multiple_of(3) {
            return Ok(IoProgress::Pending);
        }
        let count = input.len().min(7 - link.data[peer].len());
        if count == 0 {
            return Ok(IoProgress::Pending);
        }
        link.data[peer].extend(&input[..count]);
        Ok(IoProgress::Bytes(count))
    }
    fn flush(&mut self) -> Result<FlushProgress, Error> {
        Ok(FlushProgress::Complete)
    }
    fn close(&mut self) -> Result<FlushProgress, Error> {
        self.link.lock().unwrap().closed[self.side] = true;
        Ok(FlushProgress::Complete)
    }
    fn abort(&mut self) {
        let mut link = self.link.lock().unwrap();
        link.closed[self.side] = true;
        link.aborted[self.side] = true;
        link.data[self.side].clear();
    }
}

fn deadline() -> Deadline {
    Deadline::after(Tick(0), 1000).unwrap()
}
pub(crate) fn handshakes<L: Transport, R: Transport, B: TlsWireStorage, C: TlsWireStorage>(
    client: &mut TlsIo<L, B>,
    server: &mut TlsIo<R, C>,
) {
    for _ in 0..100_000 {
        let left = client.handshake().unwrap();
        let right = server.handshake().unwrap();
        if let (Some(left), Some(right)) = (left, right) {
            assert_eq!(left.peer, PeerEvidence::VerifiedServerName);
            assert_eq!(right.peer, PeerEvidence::Unauthenticated);
            return;
        }
    }
    panic!("bounded pipes did not complete the handshake");
}

struct Fault<T> {
    inner: T,
    read: Option<Result<IoProgress, Error>>,
    write: Option<Result<IoProgress, Error>>,
    flush_pending: bool,
    close_pending: bool,
    complete_at: Option<(Arc<TestClock>, u64)>,
}
impl<T> Fault<T> {
    fn new(inner: T) -> Self {
        Self {
            inner,
            read: None,
            write: None,
            flush_pending: false,
            close_pending: false,
            complete_at: None,
        }
    }
}
impl<T: Transport> Transport for Fault<T> {
    fn read(&mut self, output: &mut [u8]) -> Result<IoProgress, Error> {
        match self.read.take() {
            Some(result) => result,
            None => self.inner.read(output),
        }
    }
    fn write(&mut self, input: &[u8]) -> Result<IoProgress, Error> {
        let result = match self.write.take() {
            Some(result) => result,
            None => self.inner.write(input),
        };
        if result == Ok(IoProgress::Bytes(input.len())) {
            if let Some((clock, tick)) = self.complete_at.take() {
                clock.tick.store(tick, Ordering::SeqCst);
            }
        }
        result
    }
    fn flush(&mut self) -> Result<FlushProgress, Error> {
        if self.flush_pending {
            Ok(FlushProgress::Pending)
        } else {
            self.inner.flush()
        }
    }
    fn close(&mut self) -> Result<FlushProgress, Error> {
        if self.close_pending {
            Ok(FlushProgress::Pending)
        } else {
            self.inner.close()
        }
    }
    fn abort(&mut self) {
        self.inner.abort();
    }
}

#[test]
fn final_handshake_write_cannot_publish_past_its_deadline() {
    let clock = TestClock::new();
    let (client, server) = configs(&clock);
    let (left, right, link) = peers();
    let (mut a, mut b, mut c, mut d) = (
        [0; TLS_WIRE_BYTES],
        [0; TLS_WIRE_BYTES],
        [0; TLS_WIRE_BYTES],
        [0; TLS_WIRE_BYTES],
    );
    let mut client = TlsIo::new(
        Fault::new(left),
        TlsSession::client(client, "localhost").unwrap(),
        clock.clone(),
        deadline(),
        Deadline::after(Tick(0), 10).unwrap(),
        &mut a,
        &mut b,
    )
    .unwrap();
    let mut server = TlsIo::new(
        right,
        TlsSession::server(server).unwrap(),
        clock.clone(),
        deadline(),
        deadline(),
        &mut c,
        &mut d,
    )
    .unwrap();
    let mut armed = false;
    let mut failed = false;
    for _ in 0..100_000 {
        if client.session.status().handshake.is_some()
            && client.session.status().ciphertext_pending == 0
            && client.output_end != 0
            && !armed
        {
            client.transport.complete_at = Some((clock.clone(), 10));
            armed = true;
        }
        match client.handshake() {
            Err(Error::Deadline) => {
                failed = true;
                break;
            }
            Ok(None) => (),
            other => panic!("unexpected handshake publication: {other:?}"),
        }
        server.handshake().unwrap();
    }
    assert!(armed && failed);
    assert_eq!(client.evidence(), None);
    assert!(link.lock().unwrap().aborted[0]);
    clock.tick.store(0, Ordering::SeqCst);
    assert_eq!(client.handshake(), Err(Error::Deadline));
}

#[test]
fn flush_backpressure_withholds_finished_and_bad_transport_counts_abort() {
    let clock = TestClock::new();
    let (client_config, server_config) = configs(&clock);
    let (left, right, _) = peers();
    let (mut a, mut b, mut c, mut d) = (
        [0; TLS_WIRE_BYTES],
        [0; TLS_WIRE_BYTES],
        [0; TLS_WIRE_BYTES],
        [0; TLS_WIRE_BYTES],
    );
    let mut client = TlsIo::new(
        Fault::new(left),
        TlsSession::client(client_config.clone(), "localhost").unwrap(),
        clock.clone(),
        deadline(),
        deadline(),
        &mut a,
        &mut b,
    )
    .unwrap();
    let mut server = TlsIo::new(
        right,
        TlsSession::server(server_config.clone()).unwrap(),
        clock.clone(),
        deadline(),
        deadline(),
        &mut c,
        &mut d,
    )
    .unwrap();
    client.transport.flush_pending = true;
    for _ in 0..100_000 {
        assert_eq!(client.handshake().unwrap(), None);
        if server.handshake().unwrap().is_some() {
            break;
        }
    }
    assert!(server.evidence().is_some());
    assert!(client.session.status().handshake.is_some());
    assert_eq!(client.write(b"withheld"), Ok(IoProgress::Pending));
    assert_eq!(client.evidence(), None);
    client.transport.flush_pending = false;
    assert!(client.handshake().unwrap().is_some());
    client.transport.write = Some(Ok(IoProgress::Bytes(usize::MAX)));
    assert_eq!(client.write(b"bad count"), Err(Error::Invalid));
    assert_eq!(client.evidence(), None);

    for result in [
        Ok(IoProgress::Bytes(0)),
        Ok(IoProgress::Bytes(usize::MAX)),
        Err(Error::Deadline),
    ] {
        let (left, _, link) = peers();
        let (mut a, mut b) = ([0; TLS_WIRE_BYTES], [0; TLS_WIRE_BYTES]);
        let mut transport = Fault::new(left);
        transport.read = Some(result);
        let mut server = TlsIo::new(
            transport,
            TlsSession::server(server_config.clone()).unwrap(),
            clock.clone(),
            deadline(),
            deadline(),
            &mut a,
            &mut b,
        )
        .unwrap();
        let expected = result.err().unwrap_or(Error::Invalid);
        assert_eq!(server.handshake(), Err(expected));
        assert!(link.lock().unwrap().aborted[0]);
        assert_eq!(server.handshake(), Err(expected));
    }
}

#[test]
fn public_tls_round_trip_with_tiny_pipes_and_independent_close() {
    let clock = TestClock::new();
    let (client, server) = configs(&clock);
    let (left, right, _) = peers();
    let mut a = [0; TLS_WIRE_BYTES];
    let mut b = [0; TLS_WIRE_BYTES];
    let mut c = [0; TLS_WIRE_BYTES];
    let mut d = [0; TLS_WIRE_BYTES];
    let mut client = TlsIo::new(
        left,
        TlsSession::client(client, "localhost").unwrap(),
        clock.clone(),
        deadline(),
        deadline(),
        &mut a,
        &mut b,
    )
    .unwrap();
    let mut server = TlsIo::new(
        right,
        TlsSession::server(server).unwrap(),
        clock,
        deadline(),
        deadline(),
        &mut c,
        &mut d,
    )
    .unwrap();
    assert_eq!(client.write(b"secret").unwrap(), IoProgress::Pending);
    assert_eq!(client.evidence(), None);
    handshakes(&mut client, &mut server);
    assert_eq!(client.write(b"hello").unwrap(), IoProgress::Bytes(5));
    let mut output = [0; 5];
    let mut used = 0;
    for _ in 0..100_000 {
        client.flush().unwrap();
        if let IoProgress::Bytes(count) = server.read(&mut output[used..]).unwrap() {
            used += count;
        }
        if used == output.len() {
            break;
        }
    }
    assert_eq!(&output, b"hello");
    let mut closed = false;
    for _ in 0..100_000 {
        client.close().unwrap();
        if server.read(&mut [0]).unwrap() == IoProgress::Closed {
            closed = true;
            break;
        }
    }
    assert!(closed);
    assert_eq!(server.write(b"last").unwrap(), IoProgress::Bytes(4));
    let mut output = [0; 4];
    let mut used = 0;
    for _ in 0..100_000 {
        server.flush().unwrap();
        if let IoProgress::Bytes(count) = client.read(&mut output[used..]).unwrap() {
            used += count;
        }
        if used == output.len() {
            break;
        }
    }
    assert_eq!(&output, b"last");
    let mut closed = false;
    for _ in 0..100_000 {
        server.close().unwrap();
        if client.read(&mut [0]).unwrap() == IoProgress::Closed {
            closed = true;
            break;
        }
    }
    assert!(closed);
    assert_eq!(client.close().unwrap(), FlushProgress::Complete);
    assert_eq!(server.close().unwrap(), FlushProgress::Complete);
    assert_eq!(client.write(b"late"), Err(Error::Tls));
    assert_eq!(client.evidence(), None);
}

fn with_pair(
    work: impl FnOnce(
        &mut TlsIo<Peer, &mut [u8; TLS_WIRE_BYTES]>,
        &mut TlsIo<Peer, &mut [u8; TLS_WIRE_BYTES]>,
        &Arc<TestClock>,
    ),
) {
    let clock = TestClock::new();
    let (client, server) = configs(&clock);
    let (left, right, _) = peers();
    let mut a = [0; TLS_WIRE_BYTES];
    let mut b = [0; TLS_WIRE_BYTES];
    let mut c = [0; TLS_WIRE_BYTES];
    let mut d = [0; TLS_WIRE_BYTES];
    let mut client = TlsIo::new(
        left,
        TlsSession::client(client, "localhost").unwrap(),
        clock.clone(),
        deadline(),
        deadline(),
        &mut a,
        &mut b,
    )
    .unwrap();
    let mut server = TlsIo::new(
        right,
        TlsSession::server(server).unwrap(),
        clock.clone(),
        deadline(),
        deadline(),
        &mut c,
        &mut d,
    )
    .unwrap();
    work(&mut client, &mut server, &clock);
}

#[test]
fn simultaneous_full_chunks_progress_without_growing_wire_storage() {
    with_pair(|client, server, _| {
        handshakes(client, server);
        let left: Vec<u8> = (0..PLAIN_BYTES).map(|n| (n % 251) as u8).collect();
        let right: Vec<u8> = left.iter().map(|byte| byte ^ 0xff).collect();
        assert_eq!(client.write(&left).unwrap(), IoProgress::Bytes(left.len()));
        assert_eq!(
            server.write(&right).unwrap(),
            IoProgress::Bytes(right.len())
        );
        let mut at_client = vec![0; PLAIN_BYTES];
        let mut at_server = vec![0; PLAIN_BYTES];
        let mut left_used = 0;
        let mut right_used = 0;
        for _ in 0..200_000 {
            if left_used < at_client.len() {
                if let IoProgress::Bytes(count) = client.read(&mut at_client[left_used..]).unwrap()
                {
                    left_used += count;
                }
            } else {
                client.flush().unwrap();
            }
            if right_used < at_server.len() {
                if let IoProgress::Bytes(count) = server.read(&mut at_server[right_used..]).unwrap()
                {
                    right_used += count;
                }
            } else {
                server.flush().unwrap();
            }
            assert!(client.input_used < TLS_WIRE_BYTES && client.output_end < TLS_WIRE_BYTES);
            assert!(server.input_used < TLS_WIRE_BYTES && server.output_end < TLS_WIRE_BYTES);
            if left_used == at_client.len() && right_used == at_server.len() {
                break;
            }
        }
        assert_eq!(at_client, right);
        assert_eq!(at_server, left);
        assert_eq!(client.flush().unwrap(), FlushProgress::Complete);
        assert_eq!(server.flush().unwrap(), FlushProgress::Complete);
    });
}

#[test]
fn deadlines_and_missing_tls_time_discard_evidence_and_never_revive() {
    with_pair(|client, server, clock| {
        handshakes(client, server);
        clock.tick.store(999, Ordering::SeqCst);
        assert_eq!(client.flush().unwrap(), FlushProgress::Complete);
        clock.tick.store(1000, Ordering::SeqCst);
        assert_eq!(client.flush(), Err(Error::Deadline));
        assert_eq!(client.evidence(), None);
        assert_eq!(client.output_end, 0);
        assert!(client.transport.link.lock().unwrap().aborted[0]);
        clock.tick.store(0, Ordering::SeqCst);
        assert_eq!(client.read(&mut [0]), Err(Error::Deadline));
        assert_eq!(client.close(), Err(Error::Deadline));
        assert_eq!(client.write(b"late"), Err(Error::Deadline));
        client.abort();
        assert_eq!(client.flush(), Err(Error::Deadline));
        assert_eq!(client.read(&mut []), Ok(IoProgress::Pending));
        assert_eq!(client.write(&[]), Ok(IoProgress::Pending));
    });
    with_pair(|client, server, clock| {
        handshakes(client, server);
        clock.utc.store(-1, Ordering::SeqCst);
        assert_eq!(client.flush(), Err(Error::Tls));
        assert_eq!(client.evidence(), None);
        clock.utc.store(1_800_000_000_000, Ordering::SeqCst);
        assert_eq!(client.flush(), Err(Error::Tls));
    });
}

#[test]
fn malformed_records_truncation_and_constructor_refusal_close_the_transport() {
    let clock = TestClock::new();
    let (_, config) = configs(&clock);
    for wire in [
        vec![],
        vec![22, 3, 3],
        vec![22, 3, 3, 0x48, 0],
        vec![21, 3, 3, 0, 0],
    ] {
        let (left, _, link) = peers();
        {
            let mut link = link.lock().unwrap();
            link.data[0].extend(wire);
            link.closed[1] = true;
        }
        let mut input = [0; TLS_WIRE_BYTES];
        let mut output = [0; TLS_WIRE_BYTES];
        let mut server = TlsIo::new(
            left,
            TlsSession::server(config.clone()).unwrap(),
            clock.clone(),
            deadline(),
            deadline(),
            &mut input,
            &mut output,
        )
        .unwrap();
        let mut failure = None;
        for _ in 0..10 {
            if let Err(error) = server.handshake() {
                failure = Some(error);
                break;
            }
        }
        assert_eq!(failure, Some(Error::Tls));
        assert_eq!(server.evidence(), None);
        assert_eq!(server.input_used, 0);
        assert_eq!(server.output_end, 0);
        assert!(link.lock().unwrap().aborted[0]);
    }
    for (tick, handshake, expected) in [(1000, 1000, Error::Deadline), (0, 1001, Error::Invalid)] {
        clock.tick.store(tick, Ordering::SeqCst);
        let (left, _, link) = peers();
        let mut input = [0; TLS_WIRE_BYTES];
        let mut output = [0; TLS_WIRE_BYTES];
        let result = TlsIo::new(
            left,
            TlsSession::server(config.clone()).unwrap(),
            clock.clone(),
            deadline(),
            Deadline::after(Tick(0), handshake).unwrap(),
            &mut input,
            &mut output,
        );
        assert_eq!(result.err().map(|refusal| refusal.error()), Some(expected));
        assert!(link.lock().unwrap().aborted[0]);
    }
    for len in 0..5 {
        assert_eq!(record_length(&[0; 5][..len]), Err(Error::Tls));
    }
    for length in 0..=u16::MAX {
        let bytes = length.to_be_bytes();
        let expected = if length < 18432 {
            Ok(usize::from(length) + 5)
        } else {
            Err(Error::Tls)
        };
        assert_eq!(record_length(&[22, 3, 3, bytes[0], bytes[1]]), expected);
    }
}

#[test]
fn public_crypto_sessions_exchange_mail_bytes_over_local_tcp() {
    use crate::transport::TcpTransport;
    use std::net::{TcpListener, TcpStream};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let left = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (right, _) = listener.accept().unwrap();
    let clock = TestClock::new();
    let (client, server) = configs(&clock);
    let (mut a, mut b, mut c, mut d) = (
        [0; TLS_WIRE_BYTES],
        [0; TLS_WIRE_BYTES],
        [0; TLS_WIRE_BYTES],
        [0; TLS_WIRE_BYTES],
    );
    let mut client = TlsIo::new(
        TcpTransport::from_stream(left).unwrap(),
        TlsSession::client(client, "localhost").unwrap(),
        clock.clone(),
        deadline(),
        deadline(),
        &mut a,
        &mut b,
    )
    .unwrap();
    let mut server = TlsIo::new(
        TcpTransport::from_stream(right).unwrap(),
        TlsSession::server(server).unwrap(),
        clock,
        deadline(),
        deadline(),
        &mut c,
        &mut d,
    )
    .unwrap();
    handshakes(&mut client, &mut server);
    let mail = b"EHLO local.test\r\n";
    assert_eq!(client.write(mail).unwrap(), IoProgress::Bytes(mail.len()));
    let mut received = [0; 17];
    let mut used = 0;
    for _ in 0..100_000 {
        client.flush().unwrap();
        if let IoProgress::Bytes(count) = server.read(&mut received[used..]).unwrap() {
            used += count;
        }
        if used == received.len() {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(&received, mail);
}

#[test]
fn established_sessions_outlive_handshake_deadline_and_preserve_backpressure() {
    with_pair(|client, server, clock| {
        client.handshake_deadline = Deadline::after(Tick(0), 10).unwrap();
        server.handshake_deadline = Deadline::after(Tick(0), 10).unwrap();
        handshakes(client, server);
        clock.tick.store(20, Ordering::SeqCst);
        assert_eq!(client.flush().unwrap(), FlushProgress::Complete);
        assert_eq!(client.write(b"first").unwrap(), IoProgress::Bytes(5));
        assert!(client.output_end > 0);
        assert_eq!(client.write(b"not consumed").unwrap(), IoProgress::Pending);
        for _ in 0..100_000 {
            client.flush().unwrap();
            server.flush().unwrap();
            if server.session.status().plaintext_pending != 0 {
                break;
            }
        }
        assert_eq!(server.session.status().plaintext_pending, 5);
        let reads = server.transport.link.lock().unwrap().reads[1];
        server.flush().unwrap();
        assert_eq!(server.transport.link.lock().unwrap().reads[1], reads);
        let mut received = [0; 5];
        assert_eq!(
            server.read(&mut received[..1]).unwrap(),
            IoProgress::Bytes(1)
        );
        assert_eq!(server.transport.link.lock().unwrap().reads[1], reads);
        assert_eq!(
            server.read(&mut received[1..]).unwrap(),
            IoProgress::Bytes(4)
        );
        assert_eq!(&received, b"first");
        assert_eq!(server.read(&mut [0]).unwrap(), IoProgress::Pending);
        assert_eq!(server.write(b"still live").unwrap(), IoProgress::Bytes(10));
    });
}

#[test]
fn established_tcp_eof_without_tls_close_is_terminal_even_at_record_boundary() {
    for truncated in [false, true] {
        with_pair(|client, server, _| {
            handshakes(client, server);
            {
                let mut link = server.transport.link.lock().unwrap();
                assert!(link.data[1].is_empty());
                if truncated {
                    link.data[1].extend([23, 3, 3, 0, 17, 0]);
                }
                link.closed[0] = true;
            }
            let mut failed = false;
            for _ in 0..20 {
                match server.read(&mut [0]) {
                    Err(Error::Tls) => {
                        failed = true;
                        break;
                    }
                    Ok(IoProgress::Pending) => (),
                    other => panic!("unverified EOF published: {other:?}"),
                }
            }
            assert!(failed);
            assert_eq!(server.evidence(), None);
            assert_eq!(server.flush(), Err(Error::Tls));
        });
    }
}

#[test]
fn returned_buffers_are_reused_and_close_paths_keep_exclusive_ownership() {
    let (mut c, mut d) = ([0xa5; TLS_WIRE_BYTES], [0xa5; TLS_WIRE_BYTES]);
    reuse_buffers(&mut c, &mut d);
    reuse_buffers(Box::new(c), Box::new(d));
    owned_worker_handoff();
}

fn reuse_buffers<B: TlsWireStorage>(mut input: B, mut output: B) {
    let clock = TestClock::new();
    let (client_config, server_config) = configs(&clock);
    let (mut a, mut b) = ([0xa5; TLS_WIRE_BYTES], [0xa5; TLS_WIRE_BYTES]);
    let addresses = (input.as_ptr(), output.as_ptr());
    for _ in 0..2 {
        let (left, right, link) = peers();
        let mut client = TlsIo::new(
            left,
            TlsSession::client(client_config.clone(), "localhost").unwrap(),
            clock.clone(),
            deadline(),
            deadline(),
            &mut a,
            &mut b,
        )
        .unwrap();
        let mut server = TlsIo::new(
            Fault::new(right),
            TlsSession::server(server_config.clone()).unwrap(),
            clock.clone(),
            deadline(),
            deadline(),
            input,
            output,
        )
        .unwrap();
        handshakes(&mut client, &mut server);
        server.transport.close_pending = true;
        let mut peer_closed = false;
        for _ in 0..100_000 {
            assert_eq!(server.close().unwrap(), FlushProgress::Pending);
            if client.read(&mut [0]).unwrap() == IoProgress::Closed {
                peer_closed = true;
                break;
            }
        }
        assert!(peer_closed);
        assert!(!link.lock().unwrap().closed[1]);
        assert!(server.session.status().write_closed);
        server.transport.close_pending = false;
        assert_eq!(server.close().unwrap(), FlushProgress::Complete);
        (input, output) = server.into_buffers().unwrap();
        assert!(link.lock().unwrap().aborted[1]);
        assert_eq!((input.as_ptr(), output.as_ptr()), addresses);
    }
    let (left, _, link) = peers();
    let mut server = TlsIo::new(
        left,
        TlsSession::server(server_config.clone()).unwrap(),
        clock.clone(),
        deadline(),
        deadline(),
        input,
        output,
    )
    .unwrap();
    assert_eq!(server.close(), Err(Error::Tls));
    assert_eq!(server.evidence(), None);
    assert!(link.lock().unwrap().aborted[0]);
    let (input, output) = server.into_buffers().unwrap();
    let (left, _, link) = peers();
    let mut failed = TlsSession::server(server_config).unwrap();
    failed.abort();
    assert_eq!(
        TlsIo::new(left, failed, clock, deadline(), deadline(), input, output)
            .err()
            .map(|refusal| refusal.error()),
        Some(Error::Invalid)
    );
    assert!(link.lock().unwrap().aborted[0]);
}

#[test]
fn constructor_refusals_return_moved_pool_buffers_without_logging_them() {
    let (mut a, mut b) = ([0xa5; TLS_WIRE_BYTES], [0x5a; TLS_WIRE_BYTES]);
    refused_buffers(&mut a, &mut b);
    refused_buffers(
        Box::new([0xa5; TLS_WIRE_BYTES]),
        Box::new([0x5a; TLS_WIRE_BYTES]),
    );
}

fn refused_buffers<B: TlsWireStorage>(mut input: B, mut output: B) {
    let clock = TestClock::new();
    let (client_config, server_config) = configs(&clock);
    let addresses = (input.as_ptr(), output.as_ptr());
    for (mode, expected) in [
        (0, Error::Invalid),
        (1, Error::Deadline),
        (2, Error::Busy),
        (3, Error::Invalid),
    ] {
        let mut session = TlsSession::server(server_config.clone()).unwrap();
        let handshake_deadline = if mode == 3 {
            Deadline::after(Tick(0), 1001).unwrap()
        } else {
            deadline()
        };
        if mode == 0 {
            session.abort();
        }
        if mode == 1 {
            clock.tick.store(1000, Ordering::SeqCst);
        }
        if mode == 2 {
            clock.fail.store(true, Ordering::SeqCst);
        }
        let (left, _, link) = peers();
        let refusal = TlsIo::new(
            left,
            session,
            clock.clone(),
            deadline(),
            handshake_deadline,
            input,
            output,
        )
        .err()
        .unwrap();
        assert_eq!(refusal.error(), expected);
        assert_eq!(
            format!("{refusal:?}"),
            format!("TlsIoRefusal {{ error: {expected:?}, .. }}")
        );
        assert!(link.lock().unwrap().aborted[0]);
        (input, output) = refusal.into_buffers();
        assert_eq!((input.as_ptr(), output.as_ptr()), addresses);
        assert!(input.iter().all(|byte| *byte == 0xa5));
        assert!(output.iter().all(|byte| *byte == 0x5a));
        clock.tick.store(0, Ordering::SeqCst);
        clock.fail.store(false, Ordering::SeqCst);
    }
    let (left, right, _) = peers();
    let (mut c, mut d) = ([0; TLS_WIRE_BYTES], [0; TLS_WIRE_BYTES]);
    let mut client = TlsIo::new(
        left,
        TlsSession::client(client_config, "localhost").unwrap(),
        clock.clone(),
        deadline(),
        deadline(),
        &mut c,
        &mut d,
    )
    .unwrap();
    let mut server = TlsIo::new(
        right,
        TlsSession::server(server_config).unwrap(),
        clock,
        deadline(),
        deadline(),
        input,
        output,
    )
    .unwrap();
    handshakes(&mut client, &mut server);
    let (input, output) = server.into_buffers().unwrap();
    assert_eq!((input.as_ptr(), output.as_ptr()), addresses);
}

fn owned_worker_handoff() {
    let clock = TestClock::new();
    let (client_config, server_config) = configs(&clock);
    let (left, right, link) = peers();
    let buffers = std::array::from_fn::<_, 4, _>(|_| Box::new([0xa5; TLS_WIRE_BYTES]));
    let addresses = buffers.each_ref().map(|buffer| buffer.as_ptr());
    let [a, b, c, d] = buffers;
    let mut client = TlsIo::new(
        left,
        TlsSession::client(client_config, "localhost").unwrap(),
        clock.clone(),
        deadline(),
        deadline(),
        a,
        b,
    )
    .unwrap();
    let mut server = TlsIo::new(
        right,
        TlsSession::server(server_config).unwrap(),
        clock,
        deadline(),
        deadline(),
        c,
        d,
    )
    .unwrap();
    let buffers = std::thread::spawn(move || {
        handshakes(&mut client, &mut server);
        assert_eq!(client.write(b"owned").unwrap(), IoProgress::Bytes(5));
        let mut received = [0; 5];
        let mut used = 0;
        for _ in 0..100_000 {
            client.flush().unwrap();
            if let IoProgress::Bytes(count) = server.read(&mut received[used..]).unwrap() {
                used += count;
            }
            if used == received.len() {
                break;
            }
        }
        assert_eq!(&received, b"owned");
        let (a, b) = client.into_buffers().unwrap();
        let (c, d) = server.into_buffers().unwrap();
        [a, b, c, d]
    })
    .join()
    .unwrap();
    assert_eq!(buffers.each_ref().map(|buffer| buffer.as_ptr()), addresses);
    assert_eq!(link.lock().unwrap().aborted, [true, true]);
}
