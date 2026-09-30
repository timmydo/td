#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;

#[test]
fn server_session_public_round_trip_and_evidence() {
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let (identity, roots) = fixture();
        let clock = ClockControl::new();
        let config = Arc::new(
            ServerConfig::new(
                &[identity],
                TlsProtocol::Smtp,
                IdentitySelection::DefaultIdentity,
                None,
                clock.clock.clone(),
            )
            .unwrap(),
        );
        let mut server = TlsSession::server(config).unwrap();
        let native = rustls::ClientConfig::builder_with_details(
            Arc::new(crate::tls_policy::provider().unwrap()),
            Arc::new(crate::tls_clock::BackendClock(clock.clock)),
        )
        .with_protocol_versions(&[version])
        .unwrap()
        .with_root_certificates(roots.roots.clone())
        .with_no_client_auth();
        let mut client = rustls::Connection::Client(
            rustls::ClientConnection::new(Arc::new(native), "localhost".try_into().unwrap())
                .unwrap(),
        );
        let mut wire = Vec::new();
        for _ in 0..100 {
            wire.clear();
            while client.wants_write() {
                client.write_tls(&mut wire).unwrap();
            }
            let mut offset = 0;
            while offset < wire.len() {
                let end = offset
                    + 5
                    + usize::from(u16::from_be_bytes([wire[offset + 3], wire[offset + 4]]));
                assert_eq!(
                    server.receive_record(&wire[offset..end], false).unwrap(),
                    TlsProgress::Bytes(end - offset)
                );
                offset = end;
                let mut buffer = [0; 37];
                while server.status().ciphertext_pending != 0 {
                    let count = server.drain_ciphertext(&mut buffer).unwrap();
                    let mut input = Cursor::new(&buffer[..count]);
                    while input.position() < count as u64 {
                        client.read_tls(&mut input).unwrap();
                        client.process_new_packets().unwrap();
                    }
                }
            }
            if !client.is_handshaking() && server.status().handshake.is_some() {
                break;
            }
        }
        assert!(!client.is_handshaking());
        assert_eq!(
            server.status().handshake.unwrap().peer,
            PeerEvidence::Unauthenticated
        );
        assert_eq!(
            server.status().handshake.unwrap().version,
            if version.version == rustls::ProtocolVersion::TLSv1_2 {
                TlsVersion::V12
            } else {
                TlsVersion::V13
            }
        );
        assert_eq!(server.queue_plaintext(b"ready"), Ok(TlsProgress::Bytes(5)));
        let mut wire = [0; 128];
        let count = server.drain_ciphertext(&mut wire).unwrap();
        client.read_tls(&mut Cursor::new(&wire[..count])).unwrap();
        client.process_new_packets().unwrap();
        let mut received = [0; 5];
        client.reader().read_exact(&mut received).unwrap();
        assert_eq!(&received, b"ready");
    }
}

#[test]
fn server_session_refuses_raw_ip_sni_across_record_fragments() {
    let (identity, roots) = fixture();
    let clock = ClockControl::new();
    let client_config =
        Arc::new(ClientConfig::new(&roots, clock.clock.clone(), TlsProtocol::Smtp).unwrap());
    let mut client = TlsSession::client(client_config, "localhost").unwrap();
    let mut wire = vec![0; client.status().ciphertext_pending];
    assert_eq!(client.drain_ciphertext(&mut wire).unwrap(), wire.len());
    let length = usize::from(u16::from_be_bytes([wire[3], wire[4]]));
    assert_eq!(length + 5, wire.len());
    let offset = wire
        .windows(9)
        .position(|part| part == b"localhost")
        .unwrap();
    for replacement in [b"127.0.0.1", b"bad_name.", b"localhost", b"LoCaLhOsT"] {
        let mut mutated = wire.clone();
        mutated[offset..offset + 9].copy_from_slice(replacement);
        for fragment in [1, 2, 7, 16384] {
            for (protocol, selection) in [
                (TlsProtocol::Http1, IdentitySelection::RequiredName),
                (TlsProtocol::Smtp, IdentitySelection::DefaultIdentity),
                (TlsProtocol::Smtp, IdentitySelection::MatchPresentName),
            ] {
                let config = Arc::new(
                    ServerConfig::new(
                        std::slice::from_ref(&identity),
                        protocol,
                        selection,
                        None,
                        clock.clock.clone(),
                    )
                    .unwrap(),
                );
                let mut server = TlsSession::server(config).unwrap();
                let result = mutated[5..].chunks(fragment).try_for_each(|body| {
                    let mut record = vec![22, 3, 3];
                    record.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
                    record.extend_from_slice(body);
                    assert!(server.status().handshake.is_none());
                    server
                        .receive_record(&record, false)
                        .map(|progress| assert_eq!(progress, TlsProgress::Bytes(record.len())))
                });
                if replacement.eq_ignore_ascii_case(b"localhost") {
                    result.unwrap();
                    assert!(server.status().ciphertext_pending > 0);
                } else {
                    assert_eq!(result, Err(TlsError::Protocol));
                    assert_eq!(server.status().phase, TlsPhase::Failed);
                    assert_eq!(server.status().ciphertext_pending, 0);
                }
                assert_eq!(server.status().handshake, None);
            }
        }
    }
}

fn native_client_handshake(
    server: &mut TlsSession,
    client: &mut rustls::Connection,
) -> Result<(), TlsError> {
    let mut wire = Vec::with_capacity(128 * 1024);
    for _ in 0..100 {
        wire.clear();
        while client.wants_write() {
            client.write_tls(&mut wire).unwrap();
        }
        assert!(wire.len() <= 128 * 1024);
        let mut offset = 0;
        while offset < wire.len() {
            let end =
                offset + 5 + usize::from(u16::from_be_bytes([wire[offset + 3], wire[offset + 4]]));
            assert_eq!(
                server.receive_record(&wire[offset..end], false)?,
                TlsProgress::Bytes(end - offset)
            );
            offset = end;
            let mut buffer = [0; 37];
            while server.status().ciphertext_pending != 0 {
                let count = server.drain_ciphertext(&mut buffer)?;
                let mut input = Cursor::new(&buffer[..count]);
                while input.position() < count as u64 {
                    assert!(client.read_tls(&mut input).unwrap() > 0);
                    client.process_new_packets().map_err(native_error)?;
                }
            }
        }
        if !client.is_handshaking() && server.status().handshake.is_some() {
            return Ok(());
        }
    }
    panic!("native client handshake turn ceiling")
}

#[test]
fn server_session_mandatory_client_proof_and_typed_refusals() {
    use crate::VerificationFailure as V;
    use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
    let (identity, roots) = fixture();
    let (_, ca) = key();
    let root = f::make(&ca, &ca, &f::Parameters::new(true)).unwrap();
    let trust = TrustStore::from_pem(&pem("CERTIFICATE", &root)).unwrap();
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        for kind in [
            "valid",
            "missing",
            "foreign",
            "expired",
            "future-expired",
            "wrong-usage",
            "bad-signature",
            "too-many",
            "oversized",
            "valid",
        ] {
            let clock = ClockControl::new();
            let config = Arc::new(
                ServerConfig::new(
                    std::slice::from_ref(&identity),
                    TlsProtocol::Smtp,
                    IdentitySelection::MatchPresentName,
                    Some(&trust),
                    clock.clock.clone(),
                )
                .unwrap(),
            );
            let mut server = TlsSession::server(config).unwrap();
            let (secret, signing) = key();
            let mut parameters = f::Parameters::new(false);
            if kind != "wrong-usage" {
                parameters.extensions[3] =
                    f::extension(0x25, false, f::seq(&[f::oid(&[0x2b, 6, 1, 5, 5, 7, 3, 2])]));
            }
            if kind == "expired" {
                parameters.not_after = b"250201000000Z".to_vec();
            }
            if kind == "future-expired" {
                parameters.not_after = b"280101000000Z".to_vec();
                clock.time.store(1_900_000_000, Ordering::SeqCst);
            }
            if kind == "foreign" {
                parameters.issuer = b"foreign-root".to_vec();
            }
            if kind == "oversized" {
                parameters
                    .extensions
                    .push(f::extension(120, false, vec![0; 16384]));
            }
            let (_, other) = key();
            let signer = if kind == "foreign" { &other } else { &ca };
            let mut leaf = f::make(&signing, signer, &parameters).unwrap();
            if kind == "bad-signature" {
                *leaf.last_mut().unwrap() ^= 1;
            }
            let expected = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &leaf);
            let mut chain = vec![
                CertificateDer::from(leaf),
                CertificateDer::from(root.clone()),
            ];
            if kind == "foreign" {
                let mut foreign_root = f::Parameters::new(true);
                foreign_root.subject = b"foreign-root".to_vec();
                foreign_root.issuer = b"foreign-root".to_vec();
                chain[1] = CertificateDer::from(f::make(&other, &other, &foreign_root).unwrap());
            }
            if kind == "too-many" {
                chain.extend((0..7).map(|_| CertificateDer::from(root.clone())));
            }
            let builder = rustls::ClientConfig::builder_with_details(
                Arc::new(crate::tls_policy::provider().unwrap()),
                Arc::new(crate::tls_clock::BackendClock(clock.clock)),
            )
            .with_protocol_versions(&[version])
            .unwrap()
            .with_root_certificates(roots.roots.clone());
            let native = if kind == "missing" {
                builder.with_no_client_auth()
            } else {
                builder
                    .with_client_auth_cert(chain, PrivatePkcs8KeyDer::from(secret).into())
                    .unwrap()
            };
            let mut client = rustls::Connection::Client(
                rustls::ClientConnection::new(Arc::new(native), "localhost".try_into().unwrap())
                    .unwrap(),
            );
            assert_eq!(server.status().handshake, None);
            let result = native_client_handshake(&mut server, &mut client);
            let expected_error = match kind {
                "valid" => None,
                "missing" => Some(TlsError::Verification(V::Missing)),
                "foreign" => Some(TlsError::Verification(V::Untrusted)),
                "bad-signature" => Some(TlsError::Verification(V::Signature)),
                "expired" | "future-expired" => Some(TlsError::Verification(V::Expired)),
                "wrong-usage" => Some(TlsError::Verification(V::Usage)),
                "too-many" | "oversized" => Some(TlsError::Capacity),
                _ => panic!("unknown fixture"),
            };
            if let Some(error) = expected_error {
                assert_eq!(result, Err(error), "{kind}");
                assert_eq!(server.status().handshake, None);
                assert_eq!(server.status().ciphertext_pending, 0);
            } else {
                result.unwrap();
                let PeerEvidence::VerifiedClientLeaf(actual) =
                    server.status().handshake.unwrap().peer
                else {
                    panic!("missing verified client evidence")
                };
                assert_eq!(actual.as_slice(), expected.as_ref());
                assert_eq!(
                    server.queue_plaintext(b"accepted"),
                    Ok(TlsProgress::Bytes(8))
                );
            }
        }
    }
}

fn drain_server(server: &mut TlsSession, client: &mut rustls::Connection) {
    let mut buffer = [0; 37];
    while server.status().ciphertext_pending != 0 {
        let count = server.drain_ciphertext(&mut buffer).unwrap();
        let mut input = Cursor::new(&buffer[..count]);
        while input.position() < count as u64 {
            assert!(client.read_tls(&mut input).unwrap() > 0);
            client.process_new_packets().unwrap();
        }
    }
}

#[test]
fn server_session_valid_retry_and_fragmented_raw_retry_sni_refusal() {
    let (identity, roots) = fixture();
    for (send_sni, replacement) in [
        (true, Some(b"localhost")),
        (true, Some(b"127.0.0.1")),
        (true, Some(b"otherhost")),
        (false, None),
        (false, Some(b"127.0.0.1")),
    ] {
        for fragment in [1, 7, 16384] {
            let clock = ClockControl::new();
            let mut config = ServerConfig::new(
                std::slice::from_ref(&identity),
                TlsProtocol::Smtp,
                IdentitySelection::DefaultIdentity,
                None,
                clock.clock.clone(),
            )
            .unwrap();
            // Test-only restriction to an admitted group forces an HRR: the peer
            // offers P-384 but initially sends its preferred X25519 share.
            let mut provider = crate::tls_policy::provider().unwrap();
            provider
                .kx_groups
                .retain(|group| group.name() == rustls::NamedGroup::secp384r1);
            assert_eq!(provider.kx_groups.len(), 1);
            let mut native = rustls::ServerConfig::builder_with_details(
                Arc::new(provider),
                config.native.time_provider.clone(),
            )
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_cert_resolver(config.native.cert_resolver.clone());
            native.send_tls13_tickets = 0;
            config.native = Arc::new(native);
            let mut server = TlsSession::server(Arc::new(config)).unwrap();
            let mut provider = crate::tls_policy::provider().unwrap();
            provider.kx_groups.retain(|group| {
                matches!(
                    group.name(),
                    rustls::NamedGroup::X25519 | rustls::NamedGroup::secp384r1
                )
            });
            let mut native = rustls::ClientConfig::builder_with_details(
                Arc::new(provider),
                Arc::new(crate::tls_clock::BackendClock(clock.clock)),
            )
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots.roots.clone())
            .with_no_client_auth();
            native.enable_sni = send_sni;
            let mut client = rustls::Connection::Client(
                rustls::ClientConnection::new(Arc::new(native), "localhost".try_into().unwrap())
                    .unwrap(),
            );
            let mut first = Vec::new();
            while client.wants_write() {
                client.write_tls(&mut first).unwrap();
            }
            let mut offset = 0;
            while offset < first.len() {
                let end = offset
                    + 5
                    + usize::from(u16::from_be_bytes([first[offset + 3], first[offset + 4]]));
                assert_eq!(
                    server.receive_record(&first[offset..end], false).unwrap(),
                    TlsProgress::Bytes(end - offset)
                );
                offset = end;
                drain_server(&mut server, &mut client);
            }
            assert_eq!(server.status().handshake, None);
            let mut retry = Vec::new();
            while client.wants_write() {
                client.write_tls(&mut retry).unwrap();
            }
            assert!(!retry.is_empty());
            if send_sni {
                let indexes = retry
                    .windows(9)
                    .enumerate()
                    .filter_map(|(i, bytes)| (bytes == b"localhost").then_some(i))
                    .collect::<Vec<_>>();
                assert_eq!(indexes.len(), 1);
                retry[indexes[0]..indexes[0] + 9].copy_from_slice(replacement.unwrap());
            } else if let Some(name) = replacement {
                insert_retry_sni(&mut retry, name);
            }
            let mut offset = 0;
            let mut result = Ok(());
            while offset < retry.len() && result.is_ok() {
                let end = offset
                    + 5
                    + usize::from(u16::from_be_bytes([retry[offset + 3], retry[offset + 4]]));
                if retry[offset] == 22 {
                    for body in retry[offset + 5..end].chunks(fragment) {
                        let mut record = vec![22, 3, 3];
                        record.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
                        record.extend_from_slice(body);
                        result = server
                            .receive_record(&record, false)
                            .map(|progress| assert_eq!(progress, TlsProgress::Bytes(record.len())));
                        if result.is_err() {
                            break;
                        }
                    }
                } else {
                    result = server
                        .receive_record(&retry[offset..end], false)
                        .map(|_| ());
                }
                offset = end;
            }
            if replacement.is_none() || replacement == Some(b"localhost") {
                result.unwrap();
                drain_server(&mut server, &mut client);
                native_client_handshake(&mut server, &mut client).unwrap();
                assert_eq!(
                    client.handshake_kind(),
                    Some(rustls::HandshakeKind::FullWithHelloRetryRequest)
                );
                assert_eq!(
                    server.status().handshake.unwrap().peer,
                    PeerEvidence::Unauthenticated
                );
            } else {
                assert_eq!(result, Err(TlsError::Protocol));
                assert_eq!(server.status().handshake, None);
                assert_eq!(server.status().ciphertext_pending, 0);
            }
        }
    }
}

// Mutate only the retry's plaintext ClientHello. The negative case deliberately
// changes its transcript and must fail before any server signing/output.
fn insert_retry_sni(wire: &mut Vec<u8>, name: &[u8]) {
    let mut start = 0;
    while wire[start] != 22 {
        start += 5 + usize::from(u16::from_be_bytes([wire[start + 3], wire[start + 4]]));
    }
    let old_record = usize::from(u16::from_be_bytes([wire[start + 3], wire[start + 4]]));
    assert_eq!(wire[start + 5], 1);
    let old_handshake = usize::try_from(u32::from_be_bytes([
        0,
        wire[start + 6],
        wire[start + 7],
        wire[start + 8],
    ]))
    .unwrap();
    assert_eq!(old_record, old_handshake + 4);
    let mut ext = start + 9 + 34;
    ext += 1 + usize::from(wire[ext]);
    ext += 2 + usize::from(u16::from_be_bytes([wire[ext], wire[ext + 1]]));
    ext += 1 + usize::from(wire[ext]);
    let old_extensions = usize::from(u16::from_be_bytes([wire[ext], wire[ext + 1]]));
    assert_eq!(ext + 2 + old_extensions, start + 5 + old_record);
    let mut sni = vec![0, 0];
    sni.extend_from_slice(&u16::try_from(name.len() + 5).unwrap().to_be_bytes());
    sni.extend_from_slice(&u16::try_from(name.len() + 3).unwrap().to_be_bytes());
    sni.push(0);
    sni.extend_from_slice(&u16::try_from(name.len()).unwrap().to_be_bytes());
    sni.extend_from_slice(name);
    let growth = sni.len();
    wire.splice(ext + 2..ext + 2, sni);
    wire[ext..ext + 2].copy_from_slice(
        &u16::try_from(old_extensions + growth)
            .unwrap()
            .to_be_bytes(),
    );
    wire[start + 3..start + 5]
        .copy_from_slice(&u16::try_from(old_record + growth).unwrap().to_be_bytes());
    wire[start + 6..start + 9]
        .copy_from_slice(&u32::try_from(old_handshake + growth).unwrap().to_be_bytes()[1..]);
}

struct Pipe {
    tail: [u8; 37],
    start: usize,
    end: usize,
    bytes: VecDeque<u8>,
    record: [u8; 18437],
    used: usize,
    target: usize,
}
impl Pipe {
    fn new() -> Self {
        Self {
            tail: [0; 37],
            start: 0,
            end: 0,
            bytes: VecDeque::with_capacity(7),
            record: [0; 18437],
            used: 0,
            target: 5,
        }
    }
    fn unwritten(&self) -> bool {
        self.start != self.end
    }
    fn empty(&self) -> bool {
        !self.unwritten() && self.bytes.is_empty() && self.used == 0
    }
    fn step(
        &mut self,
        send: &mut TlsSession,
        receive: &mut TlsSession,
        receiver_tail: bool,
    ) -> Result<bool, TlsError> {
        let mut progress = false;
        if !self.unwritten() && send.status().ciphertext_pending != 0 {
            self.start = 0;
            self.end = send.drain_ciphertext(&mut self.tail)?;
            progress |= self.end != 0;
        }
        while self.bytes.len() < 7 && self.unwritten() {
            self.bytes.push_back(self.tail[self.start]);
            self.start += 1;
            progress = true;
        }
        if self.used < self.target {
            if let Some(byte) = self.bytes.pop_front() {
                self.record[self.used] = byte;
                self.used += 1;
                progress = true;
                if self.used == 5 {
                    self.target =
                        5 + usize::from(u16::from_be_bytes([self.record[3], self.record[4]]));
                    assert!(self.target <= self.record.len());
                }
            }
        }
        if self.used == self.target {
            if let TlsProgress::Bytes(count) =
                receive.receive_record(&self.record[..self.used], receiver_tail)?
            {
                assert_eq!(count, self.used);
                self.used = 0;
                self.target = 5;
                progress = true;
            }
        }
        Ok(progress)
    }
}
#[test]
fn server_session_public_pair_tiny_pipes_simultaneous_writes_and_close() {
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let (identity, roots) = fixture();
        let clock = ClockControl::new();
        let mut server_config = ServerConfig::new(
            &[identity],
            TlsProtocol::Smtp,
            IdentitySelection::DefaultIdentity,
            None,
            clock.clock.clone(),
        )
        .unwrap();
        // Restrict only the private fixture's negotiable version.
        let mut native = rustls::ServerConfig::builder_with_details(
            server_config.native.crypto_provider().clone(),
            server_config.native.time_provider.clone(),
        )
        .with_protocol_versions(&[version])
        .unwrap()
        .with_no_client_auth()
        .with_cert_resolver(server_config.native.cert_resolver.clone());
        native.send_tls13_tickets = 0;
        native.require_ems = true;
        server_config.native = Arc::new(native);
        let mut server = TlsSession::server(Arc::new(server_config)).unwrap();
        let mut client = TlsSession::client(
            Arc::new(ClientConfig::new(&roots, clock.clock, TlsProtocol::Smtp).unwrap()),
            "localhost",
        )
        .unwrap();
        let mut outward = Pipe::new();
        let mut inward = Pipe::new();
        let mut server_finished_pending = false;
        for _ in 0..100_000 {
            let a = outward
                .step(&mut client, &mut server, inward.unwritten())
                .unwrap();
            let b = inward
                .step(&mut server, &mut client, outward.unwritten())
                .unwrap();
            if server.status().handshake.is_some() && server.status().ciphertext_pending != 0 {
                server_finished_pending = true;
                assert!(!server.live.as_ref().unwrap().finished_flight_drained);
            }
            if client.status().handshake.is_some()
                && server.status().handshake.is_some()
                && outward.empty()
                && inward.empty()
                && client.status().ciphertext_pending == 0
                && server.status().ciphertext_pending == 0
            {
                break;
            }
            assert!(a || b, "public pair handshake deadlock");
        }
        if version == &rustls::version::TLS12 {
            assert!(server_finished_pending);
        }
        assert!(client.status().handshake.is_some() && server.status().handshake.is_some());
        assert!(client.live.as_ref().unwrap().finished_flight_drained);
        assert!(server.live.as_ref().unwrap().finished_flight_drained);
        assert_eq!(
            client.queue_plaintext(&[0x51; 16384]),
            Ok(TlsProgress::Bytes(16384))
        );
        assert_eq!(
            server.queue_plaintext(&[0xa2; 16384]),
            Ok(TlsProgress::Bytes(16384))
        );
        let mut at_client = 0;
        let mut at_server = 0;
        for _ in 0..100_000 {
            let a = outward
                .step(&mut client, &mut server, inward.unwritten())
                .unwrap();
            let b = inward
                .step(&mut server, &mut client, outward.unwritten())
                .unwrap();
            let mut read = false;
            for (session, count, expected) in [
                (&mut client, &mut at_client, 0xa2),
                (&mut server, &mut at_server, 0x51),
            ] {
                let mut buffer = [0; 31];
                if let TlsProgress::Bytes(n) = session.read_plaintext(&mut buffer).unwrap() {
                    assert!(buffer[..n].iter().all(|byte| *byte == expected));
                    *count += n;
                    read |= n != 0;
                }
            }
            if at_client == 16384 && at_server == 16384 {
                break;
            }
            assert!(a || b || read, "public pair data deadlock");
        }
        assert_eq!((at_client, at_server), (16384, 16384));
        client.close().unwrap();
        for _ in 0..100_000 {
            let a = outward
                .step(&mut client, &mut server, inward.unwritten())
                .unwrap();
            let b = inward
                .step(&mut server, &mut client, outward.unwritten())
                .unwrap();
            if server.status().read_closed && !server.status().write_closed {
                server.close().unwrap();
            }
            if client.status().phase == TlsPhase::Closed
                && server.status().phase == TlsPhase::Closed
                && outward.empty()
                && inward.empty()
            {
                break;
            }
            assert!(
                a || b || server.status().ciphertext_pending != 0,
                "public pair close deadlock"
            );
        }
        assert_eq!(client.status().phase, TlsPhase::Closed);
        assert_eq!(server.status().phase, TlsPhase::Closed);
        assert_eq!(server.read_plaintext(&mut [0; 1]), Ok(TlsProgress::Eof));
        assert_eq!(client.read_plaintext(&mut [0; 1]), Ok(TlsProgress::Eof));
    }
}

fn routed_fixture() -> ([Arc<ServerIdentity>; 2], TrustStore) {
    let (_, ca) = key();
    let root = f::make(&ca, &ca, &f::Parameters::new(true)).unwrap();
    let mut identities = Vec::new();
    for (name, end) in [
        ("old.test", b"280101000000Z"),
        ("new.test", b"350101000000Z"),
    ] {
        let (secret, leaf_key) = key();
        let mut parameters = f::Parameters::new(false);
        parameters.not_after = end.to_vec();
        parameters.extensions[2] =
            f::extension(0x11, false, f::seq(&[f::der(0x82, name.as_bytes())]));
        let leaf = f::make(&leaf_key, &ca, &parameters).unwrap();
        identities.push(Arc::new(
            ServerIdentity::from_pem(
                &[pem("CERTIFICATE", &leaf), pem("CERTIFICATE", &root)].concat(),
                &pem("PRIVATE KEY", &secret),
                &[name],
                Some(NOW),
            )
            .unwrap(),
        ));
    }
    (
        [identities[0].clone(), identities[1].clone()],
        TrustStore::from_pem(&pem("CERTIFICATE", &root)).unwrap(),
    )
}
fn remote_named(
    roots: &TrustStore,
    version: &'static rustls::SupportedProtocolVersion,
    name: &'static str,
) -> rustls::Connection {
    let clock = ClockControl::new();
    let native = rustls::ClientConfig::builder_with_details(
        Arc::new(crate::tls_policy::provider().unwrap()),
        Arc::new(crate::tls_clock::BackendClock(clock.clock)),
    )
    .with_protocol_versions(&[version])
    .unwrap()
    .with_root_certificates(roots.roots.clone())
    .with_no_client_auth();
    rustls::Connection::Client(
        rustls::ClientConnection::new(Arc::new(native), name.try_into().unwrap()).unwrap(),
    )
}
fn first_flight(server: &mut TlsSession, client: &mut rustls::Connection) -> Result<(), TlsError> {
    let mut bytes = Vec::new();
    while client.wants_write() {
        client.write_tls(&mut bytes).unwrap();
    }
    let mut offset = 0;
    while offset < bytes.len() {
        let end =
            offset + 5 + usize::from(u16::from_be_bytes([bytes[offset + 3], bytes[offset + 4]]));
        server.receive_record(&bytes[offset..end], false)?;
        offset = end;
        drain_server(server, client);
    }
    Ok(())
}
#[test]
fn server_session_selected_validity_and_shared_key_retirement() {
    use crate::VerificationFailure as V;
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let (identities, roots) = routed_fixture();
        let clock = ClockControl::new();
        let config = Arc::new(
            ServerConfig::new(
                &identities,
                TlsProtocol::Http1,
                IdentitySelection::RequiredName,
                None,
                clock.clock.clone(),
            )
            .unwrap(),
        );
        clock.time.store(1_900_000_000, Ordering::SeqCst);
        let mut good = TlsSession::server(config.clone()).unwrap();
        let mut client = remote_named(&roots, version, "new.test");
        native_client_handshake(&mut good, &mut client).unwrap();
        let mut expired = TlsSession::server(config.clone()).unwrap();
        let mut client = remote_named(&roots, version, "old.test");
        assert_eq!(
            native_client_handshake(&mut expired, &mut client),
            Err(TlsError::Verification(V::Expired))
        );
        assert_eq!(expired.status().handshake, None);
        assert!(matches!(
            ServerConfig::new(
                &identities,
                TlsProtocol::Http1,
                IdentitySelection::RequiredName,
                None,
                clock.clock.clone()
            ),
            Err(TlsError::Verification(V::Expired))
        ));
        // Established sessions retain their completed authentication; expiry is
        // checked at selection and Finished, while key health is always fresh.
        clock.time.store(2_100_000_000, Ordering::SeqCst);
        assert_eq!(
            good.queue_plaintext(b"still authenticated"),
            Ok(TlsProgress::Bytes(19))
        );
        clock.time.store(NOW, Ordering::SeqCst);
        let mut during = TlsSession::server(config.clone()).unwrap();
        let mut client = remote_named(&roots, version, "old.test");
        first_flight(&mut during, &mut client).unwrap();
        assert_eq!(during.status().handshake, None);
        clock.time.store(1_900_000_000, Ordering::SeqCst);
        assert_eq!(
            native_client_handshake(&mut during, &mut client),
            Err(TlsError::Verification(V::Expired))
        );
        assert_eq!(during.status().handshake, None);
        clock.time.store(NOW, Ordering::SeqCst);
        let mut established = TlsSession::server(config.clone()).unwrap();
        let mut client = remote_named(&roots, version, "old.test");
        native_client_handshake(&mut established, &mut client).unwrap();
        let mut inflight = TlsSession::server(config.clone()).unwrap();
        let mut client = remote_named(&roots, version, "old.test");
        first_flight(&mut inflight, &mut client).unwrap();
        assert_eq!(identities[0].retire_for_test(), Err(crate::Error::Crypto));
        assert!(good.drain_ciphertext(&mut [0; 128]).unwrap() > 0);
        assert_eq!(established.drain_ciphertext(&mut []), Err(TlsError::Crypto));
        assert_eq!(
            native_client_handshake(&mut inflight, &mut client),
            Err(TlsError::Crypto)
        );
        assert_eq!(inflight.status().handshake, None);
        let mut new_bad = TlsSession::server(config.clone()).unwrap();
        let mut client = remote_named(&roots, version, "old.test");
        assert_eq!(
            native_client_handshake(&mut new_bad, &mut client),
            Err(TlsError::Crypto)
        );
        let mut still_good = TlsSession::server(config.clone()).unwrap();
        let mut client = remote_named(&roots, version, "new.test");
        native_client_handshake(&mut still_good, &mut client).unwrap();
        assert_eq!(
            still_good.queue_plaintext(b"good"),
            Ok(TlsProgress::Bytes(4))
        );
        assert!(matches!(
            ServerConfig::new(
                &identities,
                TlsProtocol::Http1,
                IdentitySelection::RequiredName,
                None,
                clock.clock
            ),
            Err(TlsError::Crypto)
        ));
    }
}

#[test]
fn server_session_clock_failures_and_acceptor_unwind_retire_owned_state() {
    let (identity, _) = fixture();
    let clock = ClockControl::new();
    let config = Arc::new(
        ServerConfig::new(
            &[identity],
            TlsProtocol::Smtp,
            IdentitySelection::DefaultIdentity,
            None,
            clock.clock.clone(),
        )
        .unwrap(),
    );
    let mut server = TlsSession::server(config.clone()).unwrap();
    let observer = Arc::downgrade(&server.live.as_ref().unwrap().clock);
    assert_eq!(
        server.run::<()>(|_| panic!("synthetic accepting-state unwind")),
        Err(TlsError::Crypto)
    );
    assert!(observer.upgrade().is_none());
    assert_eq!(server.status().handshake, None);
    let mut first = TlsSession::server(config.clone()).unwrap();
    clock.time.store(0, Ordering::SeqCst);
    assert_eq!(first.read_plaintext(&mut [0; 1]), Err(TlsError::Clock));
    clock.time.store(NOW, Ordering::SeqCst);
    assert_eq!(first.close(), Err(TlsError::Clock));
    let mut second = TlsSession::server(config.clone()).unwrap();
    let mut third = TlsSession::server(config.clone()).unwrap();
    clock.time.store(u64::MAX, Ordering::SeqCst);
    assert_eq!(second.read_plaintext(&mut [0; 1]), Err(TlsError::Crypto));
    clock.time.store(NOW, Ordering::SeqCst);
    assert_eq!(third.queue_plaintext(b"blocked"), Err(TlsError::Crypto));
    assert!(matches!(TlsSession::server(config), Err(TlsError::Crypto)));
}
