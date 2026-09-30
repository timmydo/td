#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    certificate_fixtures as f, pem::tests::pem, ClockHandle, IdentitySelection, ServerConfig,
    ServerIdentity, TrustStore, UtcClock,
};
use aws_lc_rs::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};
const NOW: u64 = 1_800_000_000;
struct Source {
    time: Arc<AtomicU64>,
    calls: Arc<AtomicUsize>,
    fail_at: Arc<AtomicUsize>,
}
impl UtcClock for Source {
    fn now(&self) -> Option<u64> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let time = self.time.load(Ordering::SeqCst);
        assert_ne!(time, u64::MAX, "synthetic clock callback panic");
        (time != 0 && call != self.fail_at.load(Ordering::SeqCst)).then_some(time)
    }
}
struct ClockControl {
    time: Arc<AtomicU64>,
    calls: Arc<AtomicUsize>,
    fail_at: Arc<AtomicUsize>,
    clock: Arc<ClockHandle>,
}
impl ClockControl {
    fn new() -> Self {
        let time = Arc::new(AtomicU64::new(NOW));
        let calls = Arc::new(AtomicUsize::new(0));
        let fail_at = Arc::new(AtomicUsize::new(usize::MAX));
        let clock = Arc::new(ClockHandle::new(Source {
            time: time.clone(),
            calls: calls.clone(),
            fail_at: fail_at.clone(),
        }));
        Self {
            time,
            calls,
            fail_at,
            clock,
        }
    }
}
fn key() -> (Vec<u8>, EcdsaKeyPair) {
    let doc = EcdsaKeyPair::generate_pkcs8(
        &ECDSA_P256_SHA256_ASN1_SIGNING,
        &aws_lc_rs::rand::SystemRandom::new(),
    )
    .unwrap();
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, doc.as_ref()).unwrap();
    (doc.as_ref().to_vec(), key)
}
fn fixture() -> (Arc<ServerIdentity>, TrustStore) {
    let (_, ca) = key();
    let root = f::make(&ca, &ca, &f::Parameters::new(true)).unwrap();
    let (secret, leaf) = key();
    let leaf = f::make(&leaf, &ca, &f::Parameters::new(false)).unwrap();
    let identity = ServerIdentity::from_pem(
        &[pem("CERTIFICATE", &leaf), pem("CERTIFICATE", &root)].concat(),
        &pem("PRIVATE KEY", &secret),
        &["localhost"],
        Some(NOW),
    )
    .unwrap();
    (
        Arc::new(identity),
        TrustStore::from_pem(&pem("CERTIFICATE", &root)).unwrap(),
    )
}
struct Driver {
    client: TlsSession,
    server: rustls::Connection,
    to_server: VecDeque<u8>,
    to_client: VecDeque<u8>,
    tail: [u8; 37],
    tail_start: usize,
    tail_end: usize,
    record: [u8; 18_437],
    record_len: usize,
    record_target: usize,
    transferred: usize,
    clock: ClockControl,
}
impl Driver {
    fn new(version: &'static rustls::SupportedProtocolVersion) -> Self {
        Self::new_name(version, "localhost")
    }
    fn new_name(version: &'static rustls::SupportedProtocolVersion, name: &str) -> Self {
        Self::with_tickets(version, name, 0)
    }
    fn with_tickets(
        version: &'static rustls::SupportedProtocolVersion,
        name: &str,
        tickets: usize,
    ) -> Self {
        let (identity, roots) = fixture();
        let clock = ClockControl::new();
        let config =
            Arc::new(ClientConfig::new(&roots, clock.clock.clone(), TlsProtocol::Smtp).unwrap());
        let client = TlsSession::client(config, name).unwrap();
        let server_clock = ClockControl::new();
        let config = ServerConfig::new(
            &[identity],
            TlsProtocol::Smtp,
            IdentitySelection::DefaultIdentity,
            None,
            server_clock.clock,
        )
        .unwrap();
        let mut native = (*config.native).clone();
        // Restrict the remote protocol, retaining the owned key/verifier/clock.
        let provider = native.crypto_provider().clone();
        let mut selected =
            rustls::ServerConfig::builder_with_details(provider, native.time_provider.clone())
                .with_protocol_versions(&[version])
                .unwrap()
                .with_no_client_auth()
                .with_cert_resolver(native.cert_resolver.clone());
        // Assign a TLS 1.2 ID so the client exercises its ignored save-time path.
        selected.session_storage = rustls::server::ServerSessionMemoryCache::new(32);
        selected.ticketer = native.ticketer.clone();
        selected.send_tls13_tickets = tickets;
        selected.require_ems = true;
        // Test peer only: synthesize authenticated post-close control records.
        selected.enable_secret_extraction = true;
        native = selected;
        let mut server =
            rustls::Connection::Server(rustls::ServerConnection::new(Arc::new(native)).unwrap());
        server.set_buffer_limit(Some(32 * 1024));
        Self {
            client,
            server,
            to_server: VecDeque::with_capacity(7),
            to_client: VecDeque::with_capacity(7),
            tail: [0; 37],
            tail_start: 0,
            tail_end: 0,
            record: [0; 18_437],
            record_len: 0,
            record_target: 5,
            transferred: 0,
            clock,
        }
    }
    // Two seven-byte pipes, one-byte network reads and a 37-byte caller write
    // tail make backpressure visible independently of backend buffer capacity.
    fn step(&mut self, fail_tls12_save: bool) -> Result<bool, TlsError> {
        let mut progress = false;
        if self.tail_start == self.tail_end && self.client.status().ciphertext_pending != 0 {
            self.tail_end = self.client.drain_ciphertext(&mut self.tail)?;
            self.tail_start = 0;
            progress |= self.tail_end != 0;
        }
        while self.to_server.len() < 7 && self.tail_start < self.tail_end {
            self.to_server.push_back(self.tail[self.tail_start]);
            self.tail_start += 1;
            progress = true;
        }
        if let Some(byte) = self.to_server.pop_front() {
            assert_eq!(self.server.read_tls(&mut Cursor::new([byte])).unwrap(), 1);
            self.server.process_new_packets().map_err(native_error)?;
            self.transferred += 1;
            progress = true;
        }
        if self.server.wants_write() && self.to_client.len() < 7 {
            let available = 7 - self.to_client.len();
            let mut bytes = [0; 7];
            let count = self
                .server
                .write_tls(&mut Cursor::new(&mut bytes[..available]))
                .unwrap();
            self.to_client.extend(&bytes[..count]);
            progress |= count != 0;
        }
        if self.record_len < self.record_target {
            if let Some(byte) = self.to_client.pop_front() {
                self.record[self.record_len] = byte;
                self.record_len += 1;
                self.transferred += 1;
                progress = true;
                if self.record_len == 5 {
                    self.record_target =
                        5 + usize::from(u16::from_be_bytes([self.record[3], self.record[4]]));
                    assert!(self.record_target <= self.record.len());
                }
            }
        }
        if self.record_len == self.record_target {
            if fail_tls12_save
                && self.record[0] == 22
                && self.client.status().handshake.is_none()
                && self.client.live.as_ref().unwrap().protection == Protection::Tls12
            {
                // Operation precheck, then the backend's post-Finished save.
                self.clock.fail_at.store(
                    self.clock.calls.load(Ordering::SeqCst) + 2,
                    Ordering::SeqCst,
                );
            }
            match self.client.receive_record(
                &self.record[..self.record_len],
                self.tail_start != self.tail_end,
            )? {
                TlsProgress::Bytes(n) => {
                    assert_eq!(n, self.record_len);
                    self.record_len = 0;
                    self.record_target = 5;
                    progress = true;
                }
                TlsProgress::Blocked(_) => {}
                TlsProgress::Eof => panic!("record intake returned EOF"),
            }
        }
        assert!(self.transferred <= 256 * 1024);
        Ok(progress)
    }
    fn handshake(&mut self, fail_save: bool) -> Result<(), TlsError> {
        for _ in 0..100_000 {
            let progress = self.step(fail_save)?;
            if self.client.status().handshake.is_some()
                && !self.server.is_handshaking()
                && self.client.status().ciphertext_pending == 0
                && !self.server.wants_write()
                && self.tail_start == self.tail_end
                && self.to_client.is_empty()
                && self.to_server.is_empty()
                && self.record_len == 0
            {
                return Ok(());
            }
            assert!(
                progress,
                "bounded handshake deadlock: {:?}",
                self.client.status()
            );
        }
        panic!("handshake turn ceiling")
    }
}
#[test]
fn client_session_fragmented_handshake_and_simultaneous_writes() {
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let mut d = Driver::new(version);
        assert_eq!(d.client.status().phase, TlsPhase::Handshaking);
        assert!(matches!(
            d.client.queue_plaintext(b"credential").unwrap(),
            TlsProgress::Blocked(_)
        ));
        d.handshake(false).unwrap();
        assert_eq!(d.client.status().phase, TlsPhase::Open);
        assert_eq!(
            d.client.status().handshake.unwrap().peer,
            PeerEvidence::VerifiedServerName
        );
        assert_eq!(
            d.client.queue_plaintext(&[0x51; 16_384]).unwrap(),
            TlsProgress::Bytes(16_384)
        );
        assert_eq!(
            d.client.queue_plaintext(b"extra").unwrap(),
            TlsProgress::Blocked(BlockedOn::DrainCiphertext)
        );
        d.server.writer().write_all(&[0x72; 16_384]).unwrap();
        let (mut received_client, mut received_server) = (0, 0);
        for _ in 0..100_000 {
            let mut progress = d.step(false).unwrap();
            let mut bytes = [0; 113];
            if let TlsProgress::Bytes(count) = d.client.read_plaintext(&mut bytes).unwrap() {
                assert!(bytes[..count].iter().all(|byte| *byte == 0x72));
                received_client += count;
                progress |= count != 0;
            }
            match d.server.reader().read(&mut bytes) {
                Ok(count) => {
                    assert!(bytes[..count].iter().all(|byte| *byte == 0x51));
                    received_server += count;
                    progress |= count != 0;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                other => panic!("unexpected read: {other:?}"),
            }
            if received_client == 16_384 && received_server == 16_384 {
                break;
            }
            assert!(progress || d.client.status().plaintext_pending != 0);
        }
        assert_eq!((received_client, received_server), (16_384, 16_384));
    }
}
#[test]
fn client_session_retains_ignored_tls12_clock_failure() {
    let mut d = Driver::new(&rustls::version::TLS12);
    assert_eq!(d.handshake(true), Err(TlsError::Clock));
    assert_eq!(d.clock.clock.now(), Ok(NOW));
    assert_eq!(d.client.status().phase, TlsPhase::Failed);
    assert_eq!(d.client.status().handshake, None);
    assert_eq!(
        d.client.drain_ciphertext(&mut [0; 128]),
        Err(TlsError::Clock)
    );
    assert_eq!(d.client.queue_plaintext(b"secret"), Err(TlsError::Clock));
}
#[test]
fn client_session_checks_time_without_native_time_requests() {
    let mut d = Driver::new(&rustls::version::TLS13);
    d.handshake(false).unwrap();
    d.clock.time.store(0, Ordering::SeqCst);
    assert_eq!(d.client.queue_plaintext(b"secret"), Err(TlsError::Clock));
    d.clock.time.store(NOW, Ordering::SeqCst);
    assert_eq!(d.client.read_plaintext(&mut [0; 8]), Err(TlsError::Clock));
    assert_eq!(d.client.status().handshake, None);
}

fn peer_close_record(d: &mut Driver) -> Vec<u8> {
    d.server.send_close_notify();
    let mut wire = [0; 18_437];
    let size = d
        .server
        .write_tls(&mut Cursor::new(wire.as_mut_slice()))
        .unwrap();
    assert!(!d.server.wants_write());
    assert_eq!(
        size,
        5 + usize::from(u16::from_be_bytes([wire[3], wire[4]]))
    );
    wire[..size].to_vec()
}
#[test]
fn client_session_close_halves_and_transport_eof() {
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let mut d = Driver::new(version);
        d.handshake(false).unwrap();
        let record = peer_close_record(&mut d);
        assert_eq!(
            d.client.receive_record(&record, false).unwrap(),
            TlsProgress::Bytes(record.len())
        );
        assert!(d.client.status().read_closed);
        assert_eq!(d.client.read_plaintext(&mut []), Ok(TlsProgress::Eof));
        assert_eq!(d.client.transport_eof(), Ok(()));
        assert_eq!(
            d.client.status().write_closed,
            version.version == rustls::ProtocolVersion::TLSv1_2
        );
        if version.version == rustls::ProtocolVersion::TLSv1_3 {
            assert!(d.client.status().write_ready);
            assert_eq!(
                d.client.queue_plaintext(b"after peer close"),
                Ok(TlsProgress::Bytes(16))
            );
            d.client.close().unwrap();
        }
        let queued = d.client.status().ciphertext_pending;
        assert!(queued > 0);
        d.client.close().unwrap();
        assert_eq!(d.client.status().ciphertext_pending, queued);
        for _ in 0..10_000 {
            if d.client.status().ciphertext_pending == 0
                && d.tail_start == d.tail_end
                && d.to_server.is_empty()
            {
                break;
            }
            assert!(d.step(false).unwrap());
        }
        assert_eq!(d.client.status().phase, TlsPhase::Closed);
        assert_eq!(d.tail_start, d.tail_end);
        assert!(d.to_server.is_empty());
        if version.version == rustls::ProtocolVersion::TLSv1_3 {
            let mut output = [0; 16];
            d.server.reader().read_exact(&mut output).unwrap();
            assert_eq!(&output, b"after peer close");
        }
        // Received close makes later framed input an explicit discard. It may
        // not reopen reads or be passed into the backend's zero-byte read path.
        let junk = [23, 3, 3, 0, 1, 0xff];
        assert_eq!(
            d.client.receive_record(&junk, false),
            Ok(TlsProgress::Bytes(junk.len()))
        );
        assert_eq!(d.client.status().phase, TlsPhase::Closed);
        assert_eq!(
            d.client.receive_record(&junk[..5], false),
            Err(TlsError::Protocol)
        );
        assert_eq!(d.client.status().handshake, None);
    }
    let mut d = Driver::new(&rustls::version::TLS13);
    d.handshake(false).unwrap();
    assert_eq!(d.client.transport_eof(), Err(TlsError::Protocol));
}
#[test]
fn client_session_tls12_alert_refuses_pending_ciphertext_and_socket_tail() {
    for caller_tail in [false, true] {
        let mut d = Driver::new(&rustls::version::TLS12);
        d.handshake(false).unwrap();
        assert_eq!(
            d.client.queue_plaintext(b"pending"),
            Ok(TlsProgress::Bytes(7))
        );
        if caller_tail {
            assert!(d.client.drain_ciphertext(&mut [0; 128]).unwrap() > 0);
            assert_eq!(d.client.status().ciphertext_pending, 0);
        }
        let record = peer_close_record(&mut d);
        assert_eq!(
            d.client.receive_record(&record, caller_tail),
            Err(TlsError::Protocol)
        );
        assert_eq!(d.client.status().ciphertext_pending, 0);
        assert_eq!(d.client.status().handshake, None);
    }
    let mut d = Driver::new(&rustls::version::TLS13);
    d.handshake(false).unwrap();
    d.client.queue_plaintext(b"pending").unwrap();
    let pending = d.client.status().ciphertext_pending;
    let record = peer_close_record(&mut d);
    assert_eq!(
        d.client.receive_record(&record, true),
        Ok(TlsProgress::Bytes(record.len()))
    );
    assert_eq!(d.client.status().ciphertext_pending, pending);
    assert!(!d.client.status().write_closed);
}
#[test]
fn client_session_never_emits_after_close_or_reuses_failed_state() {
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        for bad_mac in [false, true] {
            let mut d = Driver::new(version);
            d.handshake(false).unwrap();
            let config = d.client.live.as_ref().unwrap().client_config();
            d.client.close().unwrap();
            assert!(d.client.drain_ciphertext(&mut [0; 128]).unwrap() > 0);
            let error = if bad_mac {
                d.server.writer().write_all(b"bad authentication").unwrap();
                let mut wire = [0; 128];
                let size = d
                    .server
                    .write_tls(&mut Cursor::new(wire.as_mut_slice()))
                    .unwrap();
                wire[size - 1] ^= 1;
                d.client.receive_record(&wire[..size], false).unwrap_err()
            } else {
                d.client
                    .receive_record(&[23, 3, 3, 0x48, 0], false)
                    .unwrap_err()
            };
            assert!(matches!(error, TlsError::Protocol | TlsError::Crypto));
            assert_eq!(d.client.status().ciphertext_pending, 0);
            assert_eq!(d.client.status().handshake, None);
            assert_eq!(d.client.drain_ciphertext(&mut [0; 128]), Err(error));
            d.client.abort();
            assert_eq!(d.client.status().error, Some(error));
            assert!(TlsSession::client(config, "localhost").is_ok());
        }
    }
    let mut d = Driver::new(&rustls::version::TLS13);
    d.handshake(false).unwrap();
    d.client.close().unwrap();
    assert!(d.client.status().ciphertext_pending > 0);
    assert_eq!(
        d.client.queue_plaintext(b"forbidden"),
        Err(TlsError::Invalid)
    );
    assert_eq!(d.client.status().ciphertext_pending, 0);
}

#[test]
fn client_session_unwind_consumes_state_and_shared_clock_failure_fences_reuse() {
    let mut d = Driver::new(&rustls::version::TLS13);
    d.handshake(false).unwrap();
    let observer = Arc::downgrade(&d.client.live.as_ref().unwrap().clock);
    let config = d.client.live.as_ref().unwrap().client_config();
    assert_eq!(
        d.client.run::<()>(|_| panic!("synthetic session unwind")),
        Err(TlsError::Crypto)
    );
    assert!(observer.upgrade().is_none());
    assert_eq!(d.client.status().handshake, None);
    assert!(TlsSession::client(config, "localhost").is_ok());

    let mut d = Driver::new(&rustls::version::TLS13);
    d.handshake(false).unwrap();
    let config = d.client.live.as_ref().unwrap().client_config();
    let mut second = TlsSession::client(config.clone(), "localhost").unwrap();
    d.clock.time.store(u64::MAX, Ordering::SeqCst);
    assert_eq!(d.client.drain_ciphertext(&mut []), Err(TlsError::Crypto));
    d.clock.time.store(NOW, Ordering::SeqCst);
    assert_eq!(second.drain_ciphertext(&mut [0; 37]), Err(TlsError::Crypto));
    assert!(matches!(
        TlsSession::client(config, "localhost"),
        Err(TlsError::Crypto)
    ));
    assert_eq!(second.status().handshake, None);
    assert_eq!(second.status().ciphertext_pending, 0);
}

#[test]
fn client_session_unread_plaintext_blocks_without_consuming_next_record() {
    let mut d = Driver::new(&rustls::version::TLS13);
    d.handshake(false).unwrap();
    assert_eq!(d.client.queue_plaintext(&[]), Ok(TlsProgress::Bytes(0)));
    assert_eq!(d.client.read_plaintext(&mut []), Ok(TlsProgress::Bytes(0)));
    assert_eq!(d.client.drain_ciphertext(&mut []), Ok(0));
    d.server.writer().write_all(b"first").unwrap();
    d.server.writer().write_all(b"second").unwrap();
    for _ in 0..1_000 {
        if !d.step(false).unwrap() {
            break;
        }
    }
    assert_eq!(d.client.status().plaintext_pending, 5);
    assert!(!d.client.status().wants_input);
    assert_eq!(d.record_len, d.record_target);
    assert!(d.record_len > 5);
    let before = d.client.status();
    assert_eq!(
        d.client.receive_record(&d.record[..d.record_len], false),
        Ok(TlsProgress::Blocked(BlockedOn::DrainPlaintext))
    );
    assert_eq!(d.client.status(), before);
    let mut first = [0; 5];
    assert_eq!(
        d.client.read_plaintext(&mut first),
        Ok(TlsProgress::Bytes(5))
    );
    assert_eq!(&first, b"first");
    assert!(d.client.status().wants_input);
    assert!(d.step(false).unwrap());
    let mut second = [0; 6];
    assert_eq!(
        d.client.read_plaintext(&mut second[..2]),
        Ok(TlsProgress::Bytes(2))
    );
    assert_eq!(
        d.client.read_plaintext(&mut second[2..]),
        Ok(TlsProgress::Bytes(4))
    );
    assert_eq!(&second, b"second");
    assert_eq!(d.client.status().plaintext_pending, 0);
}

#[test]
fn client_session_name_verification_maps_and_retires_without_output() {
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let mut d = Driver::new_name(version, "wrong.mail.test");
        let config = d.client.live.as_ref().unwrap().client_config();
        let calls = d.clock.calls.load(Ordering::SeqCst);
        for name in ["", "127.0.0.1", "localhost.", "*.mail.test"] {
            assert!(matches!(
                TlsSession::client(config.clone(), name),
                Err(TlsError::Invalid)
            ));
        }
        assert_eq!(d.clock.calls.load(Ordering::SeqCst), calls);
        let error = TlsError::Verification(crate::VerificationFailure::Name);
        assert_eq!(d.handshake(false), Err(error));
        assert_eq!(d.client.status().error, Some(error));
        assert_eq!(d.client.status().handshake, None);
        assert_eq!(d.client.status().ciphertext_pending, 0);
        assert_eq!(d.client.read_plaintext(&mut [0; 32]), Err(error));
        assert!(TlsSession::client(config, "localhost").is_ok());
    }
}

#[test]
fn client_session_key_update_response_advances_without_application_write() {
    for application_pending in [false, true] {
        let mut d = Driver::new(&rustls::version::TLS13);
        d.handshake(false).unwrap();
        if application_pending {
            d.client.queue_plaintext(b"old key").unwrap();
        }
        d.server.refresh_traffic_keys().unwrap();
        let mut wire = [0; 128];
        let size = d
            .server
            .write_tls(&mut Cursor::new(wire.as_mut_slice()))
            .unwrap();
        let pending = d.client.status().ciphertext_pending;
        assert_eq!(
            d.client.receive_record(&wire[..size], false),
            Ok(TlsProgress::Bytes(size))
        );
        assert!(d.client.status().ciphertext_pending > pending);
        for _ in 0..1_000 {
            if d.client.status().ciphertext_pending == 0
                && d.tail_start == d.tail_end
                && d.to_server.is_empty()
            {
                break;
            }
            assert!(d.step(false).unwrap());
        }
        if application_pending {
            let mut old = [0; 7];
            d.server.reader().read_exact(&mut old).unwrap();
            assert_eq!(&old, b"old key");
        }
        assert_eq!(
            d.client.queue_plaintext(b"new key"),
            Ok(TlsProgress::Bytes(7))
        );
        for _ in 0..1_000 {
            if d.client.status().ciphertext_pending == 0
                && d.tail_start == d.tail_end
                && d.to_server.is_empty()
            {
                break;
            }
            assert!(d.step(false).unwrap());
        }
        let mut new = [0; 7];
        d.server.reader().read_exact(&mut new).unwrap();
        assert_eq!(&new, b"new key");
    }
}

#[test]
fn client_session_handshake_reassembly_counts_retained_record_headers() {
    fn client() -> TlsSession {
        let mut d = Driver::new(&rustls::version::TLS13);
        assert!(d.client.drain_ciphertext(&mut [0; 4096]).unwrap() > 0);
        assert_eq!(d.client.status().ciphertext_pending, 0);
        d.client
    }
    for fragments in [vec![vec![2, 1, 0, 0]], vec![vec![2, 1], vec![0, 0]]] {
        let mut session = client();
        let last = fragments.len() - 1;
        for (i, body) in fragments.iter().enumerate() {
            let mut wire = vec![22, 3, 3, 0, u8::try_from(body.len()).unwrap()];
            wire.extend(body);
            let result = session.receive_record(&wire, false);
            if i == last {
                assert_eq!(result, Err(TlsError::Capacity));
            } else {
                assert_eq!(result, Ok(TlsProgress::Bytes(wire.len())));
            }
        }
        assert_eq!(session.status().handshake, None);
    }
    // Total retained storage includes four handshake-header bytes and five
    // bytes for every record, even after payload coalescing moves the bytes.
    for (fragment, boundary) in [(16_384, 65_511usize), (4_096, 65_451usize)] {
        for (length, error) in [
            (boundary, TlsError::Protocol),
            (boundary + 1, TlsError::Capacity),
        ] {
            let mut session = client();
            let encoded = u32::try_from(length).unwrap().to_be_bytes();
            let mut handshake = vec![0; length + 4];
            handshake[0] = 2;
            handshake[1..4].copy_from_slice(&encoded[1..]);
            let mut refusal = None;
            for body in handshake.chunks(fragment) {
                let mut wire = vec![22, 3, 3];
                wire.extend(u16::try_from(body.len()).unwrap().to_be_bytes());
                wire.extend(body);
                match session.receive_record(&wire, false) {
                    Ok(TlsProgress::Bytes(count)) => assert_eq!(count, wire.len()),
                    Err(error) => {
                        refusal = Some(error);
                        break;
                    }
                    other => panic!("unexpected reassembly result: {other:?}"),
                }
            }
            // At the limit decoding reaches the deliberately invalid hello;
            // one byte above exhausts retained capacity before it can decode.
            assert_eq!(
                refusal,
                Some(error),
                "fragment={fragment}, payload={length}"
            );
            assert_eq!(session.status().phase, TlsPhase::Failed);
        }
    }
}

#[test]
fn client_session_errors_are_fixed_and_do_not_export_backend_diagnostics() {
    use crate::VerificationFailure as V;
    use rustls::{CertificateError as C, Error as E};
    for (input, output) in [
        (
            E::NoCertificatesPresented,
            TlsError::Verification(V::Missing),
        ),
        (
            E::InvalidCertificate(C::UnknownIssuer),
            TlsError::Verification(V::Untrusted),
        ),
        (
            E::InvalidCertificate(C::Expired),
            TlsError::Verification(V::Expired),
        ),
        (
            E::InvalidCertificate(C::NotValidYet),
            TlsError::Verification(V::NotYetValid),
        ),
        (
            E::InvalidCertificate(C::NotValidForName),
            TlsError::Verification(V::Name),
        ),
        (
            E::InvalidCertificate(C::InvalidPurpose),
            TlsError::Verification(V::Usage),
        ),
        (
            E::InvalidCertificate(C::BadSignature),
            TlsError::Verification(V::Signature),
        ),
        (
            E::InvalidCertificate(C::BadEncoding),
            TlsError::Verification(V::Other),
        ),
        (E::DecryptError, TlsError::Protocol),
        (E::EncryptError, TlsError::Crypto),
        (E::FailedToGetRandomBytes, TlsError::Crypto),
        (E::FailedToGetCurrentTime, TlsError::Clock),
        (
            E::Other(rustls::OtherError(Arc::new(TlsError::Capacity))),
            TlsError::Capacity,
        ),
        (
            E::Other(rustls::OtherError(Arc::new(std::io::Error::other(
                "private backend diagnostic",
            )))),
            TlsError::Crypto,
        ),
        (
            E::General("private backend diagnostic".into()),
            TlsError::Crypto,
        ),
    ] {
        let actual = native_error(input);
        assert_eq!(actual, output);
        assert!(!format!("{actual:?}: {actual}").contains("private backend diagnostic"));
        assert!(std::error::Error::source(&actual).is_none());
    }
}

#[test]
fn client_session_ticket_clock_failure_after_finished_is_terminal() {
    let mut d = Driver::with_tickets(&rustls::version::TLS13, "localhost", 2);
    let mut armed = false;
    let mut failed = false;
    for _ in 0..100_000 {
        if !armed
            && d.client.status().handshake.is_some()
            && !d.server.is_handshaking()
            && d.record_len > 5
            && d.record_len + 1 == d.record_target
        {
            assert_eq!(d.client.status().ciphertext_pending, 0);
            assert_eq!(d.tail_start, d.tail_end);
            // Record operation precheck, then the native ticket-time callback.
            d.clock
                .fail_at
                .store(d.clock.calls.load(Ordering::SeqCst) + 2, Ordering::SeqCst);
            armed = true;
        }
        match d.step(false) {
            Err(TlsError::Clock) => {
                failed = true;
                break;
            }
            Ok(progress) => assert!(progress),
            other => panic!("unexpected ticket result: {other:?}"),
        }
    }
    assert!(armed && failed);
    assert_eq!(d.clock.clock.now(), Ok(NOW));
    assert_eq!(d.client.status().phase, TlsPhase::Failed);
    assert_eq!(d.client.status().handshake, None);
    assert_eq!(d.client.status().ciphertext_pending, 0);
    assert_eq!(
        d.client.queue_plaintext(b"credentials"),
        Err(TlsError::Clock)
    );
}

#[test]
fn client_session_unfinished_cancellation_eof_and_alert_refuse_evidence() {
    let mut d = Driver::new(&rustls::version::TLS13);
    assert_eq!(
        d.client.read_plaintext(&mut [0; 1]),
        Ok(TlsProgress::Blocked(BlockedOn::DrainCiphertext))
    );
    assert_eq!(
        d.client.queue_plaintext(b"secret"),
        Ok(TlsProgress::Blocked(BlockedOn::DrainCiphertext))
    );
    assert_eq!(d.client.status().handshake, None);
    assert_eq!(d.client.close(), Err(TlsError::Invalid));
    d.client.abort();
    assert_eq!(d.client.close(), Err(TlsError::Invalid));
    let mut d = Driver::new(&rustls::version::TLS13);
    assert_eq!(d.client.transport_eof(), Err(TlsError::Protocol));
    d.client.abort();
    assert_eq!(d.client.status().error, Some(TlsError::Protocol));
    let mut d = Driver::new(&rustls::version::TLS12);
    assert_eq!(
        d.client.receive_record(&[21, 3, 3, 0, 2, 1, 0], false),
        Err(TlsError::Protocol)
    );
    assert_eq!(d.client.status().handshake, None);
    let mut d = Driver::new(&rustls::version::TLS13);
    d.client.abort();
    d.client.abort();
    assert_eq!(d.client.status().error, Some(TlsError::Invalid));
    assert_eq!(d.client.status().ciphertext_pending, 0);
}

#[test]
fn client_session_tls12_hello_request_never_adds_post_close_output() {
    use aws_lc_rs::aead;
    for close_state in [0, 1, 2, 3] {
        let mut d = Driver::new(&rustls::version::TLS12);
        d.handshake(false).unwrap();
        let Driver {
            mut client, server, ..
        } = d;
        let secrets = server.dangerous_extract_secrets().unwrap();
        let (sequence, traffic) = secrets.tx;
        let rustls::ConnectionTrafficSecrets::Aes256Gcm { key, iv } = traffic else {
            panic!("expected pinned AES-256-GCM fixture suite");
        };
        let key = aead::LessSafeKey::new(
            aead::UnboundKey::new(&aead::AES_256_GCM, key.as_ref()).unwrap(),
        );
        let explicit = sequence.to_be_bytes();
        let mut nonce = [0; 12];
        nonce[..4].copy_from_slice(&iv.as_ref()[..4]);
        nonce[4..].copy_from_slice(&explicit);
        let mut aad = sequence.to_be_bytes().to_vec();
        aad.extend_from_slice(&[22, 3, 3, 0, 4]);
        let mut message = vec![0, 0, 0, 0]; // TLS 1.2 HelloRequest.
        key.seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(aad),
            &mut message,
        )
        .unwrap();
        let mut record = vec![22, 3, 3, 0, 28];
        record.extend_from_slice(&explicit);
        record.extend_from_slice(&message);
        if close_state != 0 {
            client.close().unwrap();
            if close_state == 2 {
                assert_eq!(client.drain_ciphertext(&mut [0; 8]).unwrap(), 8);
            }
            if close_state == 3 {
                assert!(client.drain_ciphertext(&mut [0; 128]).unwrap() > 0);
            }
        }
        let result = client.receive_record(&record, close_state == 2);
        if close_state == 0 {
            assert_eq!(result, Ok(TlsProgress::Bytes(record.len())));
            assert!(client.status().ciphertext_pending > 0); // Valid request/response baseline.
        } else {
            assert_eq!(result, Err(TlsError::Protocol));
            assert_eq!(client.status().ciphertext_pending, 0);
            assert_eq!(client.status().handshake, None);
            assert_eq!(
                client.drain_ciphertext(&mut [0; 128]),
                Err(TlsError::Protocol)
            );
        }
    }
}

#[test]
fn client_session_tls12_simultaneous_close_pins_pending_output_refusal() {
    for caller_tail in [false, true] {
        let mut d = Driver::new(&rustls::version::TLS12);
        d.handshake(false).unwrap();
        d.client.close().unwrap();
        if caller_tail {
            assert!(d.client.drain_ciphertext(&mut [0; 128]).unwrap() > 0);
        }
        let record = peer_close_record(&mut d);
        assert_eq!(
            d.client.receive_record(&record, caller_tail),
            Err(TlsError::Protocol)
        );
        assert_eq!(d.client.status().phase, TlsPhase::Failed);
        assert_eq!(d.client.status().ciphertext_pending, 0);
    }
}

impl Live {
    fn client_config(&self) -> Arc<ClientConfig> {
        match &self.config {
            Role::Client(config) => config.clone(),
            Role::Server(_) => panic!("expected client fixture"),
        }
    }
}

#[path = "tls_server_session_tests.rs"]
mod server;
