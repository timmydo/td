use super::*;
use crate::ports::{Deadline, Handshake, PeerVerification, TlsTransport};

fn acme_source() -> String {
    source(false).replace(
        "mode = \"files\"\nchain_file = \"/chain\"\nkey_file = \"/key\"",
        "mode = \"acme\"",
    ) + r#"[acme]
directory = "https://localhost/directory"
contact = "main@example.test"
terms_accepted = true
ca_file = "/acme-ca"
[listener "http01"]
kind = "http01"
bind = "127.0.0.1:80"
"#
}

#[test]
fn client_only_acme_bootstrap_never_opens_server_material() {
    let _serial = serial();
    let material = Material::new();
    let configuration = resolved(&acme_source());
    let mut set = GenerationSet::at_startup();
    let mut requests = Vec::new();
    let prepared = TlsPolicies::prepare_clients(
        set.reserve().unwrap(),
        &configuration,
        material.clock.clone(),
        |r| {
            assert!(matches!(
                r.kind(),
                MaterialKind::RelayCa | MaterialKind::AcmeCa
            ));
            requests.push(r.kind());
            material.open(r)
        },
    )
    .unwrap();
    drop(set.publish(prepared).unwrap());
    assert_eq!(requests, [MaterialKind::RelayCa, MaterialKind::AcmeCa]);
    let current = set.current().unwrap();
    assert_eq!(current.value().coverage(), PolicyCoverage::ClientsOnly);
    assert_eq!(current.value().len(), 2);
    assert_eq!(
        TlsPolicies::listener(&current, "smtp"),
        Err(Error::NotFound)
    );
    assert_eq!(
        TlsPolicies::listener(&current, "https"),
        Err(Error::NotFound)
    );
    assert_eq!(
        TlsPolicies::role(&current, TlsPolicies::relay(&current).unwrap()),
        Ok(PolicyRole::Relay)
    );
    assert_eq!(
        TlsPolicies::role(&current, TlsPolicies::acme(&current).unwrap()),
        Ok(PolicyRole::Acme)
    );
    let failed = TlsPolicies::prepare(
        set.reserve().unwrap(),
        &configuration,
        material.clock.clone(),
        |r| {
            if matches!(r.kind(), MaterialKind::Chain | MaterialKind::Key) {
                Err(Error::NotFound)
            } else {
                material.open(r)
            }
        },
    )
    .unwrap_err();
    assert_eq!(failed, Error::NotFound);
    assert_eq!(set.current().unwrap().id(), current.id());
    assert_eq!(set.available(), 1);
}

struct FutureClock;
impl Clock for FutureClock {
    fn sample(&self) -> Result<Time, Error> {
        Ok(Time {
            utc_ms: 2_300_000_000_000,
            monotonic: Tick(0),
        })
    }
}

#[test]
fn expired_server_does_not_block_clients_but_invalid_client_trust_still_refuses() {
    let _serial = serial();
    let mut material = Material::new();
    material.clock = Arc::new(ClockHandle::new(TlsClockSource::new(Arc::new(FutureClock))));
    let configuration = resolved(&acme_source());
    let mut set = GenerationSet::at_startup();
    assert_eq!(
        TlsPolicies::prepare(
            set.reserve().unwrap(),
            &configuration,
            material.clock.clone(),
            |r| material.open(r)
        )
        .unwrap_err(),
        Error::Tls
    );
    let prepared = TlsPolicies::prepare_clients(
        set.reserve().unwrap(),
        &configuration,
        material.clock.clone(),
        |r| material.open(r),
    )
    .unwrap();
    drop(set.publish(prepared).unwrap());
    let current = set.current().unwrap();
    let id = TlsPolicies::relay(&current).unwrap();
    let pool = HandshakePool::new(1).unwrap();
    let (input, output) = buffers();
    let session = TlsPolicies::reserve_session(current.clone(), id, &pool, input, output)
        .unwrap()
        .construct()
        .unwrap();
    assert_eq!(session.status().phase, TlsPhase::Handshaking);
    drop(session.into_buffers());
    for kind in [MaterialKind::RelayCa, MaterialKind::AcmeCa] {
        for missing in [false, true] {
            let failed = TlsPolicies::prepare_clients(
                set.reserve().unwrap(),
                &configuration,
                material.clock.clone(),
                |r| {
                    if r.kind() == kind {
                        if missing {
                            Err(Error::NotFound)
                        } else {
                            Ok(Cursor::new(b"bad CA".as_slice()))
                        }
                    } else {
                        material.open(r)
                    }
                },
            )
            .unwrap_err();
            assert_eq!(failed, if missing { Error::NotFound } else { Error::Tls });
            assert_eq!(set.available(), 1);
            assert_eq!(set.current().unwrap().id(), current.id());
        }
    }
}

#[test]
fn client_generation_transitions_to_complete_within_the_same_two_slots() {
    use crate::transport::TcpTransport;
    use std::net::{TcpListener, TcpStream};
    let _serial = serial();
    let material = Material::new();
    let configuration = resolved(&source(true));
    let mut set = GenerationSet::at_startup();
    let clients = TlsPolicies::prepare_clients(
        set.reserve().unwrap(),
        &configuration,
        material.clock.clone(),
        |r| {
            assert_eq!(r.kind(), MaterialKind::RelayCa);
            material.open(r)
        },
    )
    .unwrap();
    drop(set.publish(clients).unwrap());
    let old = set.current().unwrap();
    assert_eq!(old.value().len(), 1);
    assert_eq!(TlsPolicies::acme(&old), Err(Error::NotFound));
    assert_eq!(TlsPolicies::listener(&old, "gateway"), Err(Error::NotFound));
    let relay = TlsPolicies::relay(&old).unwrap();
    let pool = HandshakePool::new(2).unwrap();
    let (input, output) = buffers();
    let outgoing = TlsPolicies::reserve_session(old, relay, &pool, input, output)
        .unwrap()
        .construct()
        .unwrap();
    let complete = TlsPolicies::prepare(
        set.reserve().unwrap(),
        &configuration,
        material.clock.clone(),
        |r| material.open(r),
    )
    .unwrap();
    drop(set.publish(complete).unwrap());
    let current = set.current().unwrap();
    assert_eq!(current.value().coverage(), PolicyCoverage::Complete);
    assert_eq!(TlsPolicies::role(&current, relay), Err(Error::Conflict));
    assert_eq!(set.available(), 0);
    assert_eq!(set.reserve().unwrap_err(), Error::Busy);
    let server_id = TlsPolicies::listener(&current, "smtp").unwrap();
    let (input, output) = buffers();
    let incoming = TlsPolicies::reserve_session(current, server_id, &pool, input, output)
        .unwrap()
        .construct()
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    let cap = Deadline::after(Tick(0), 1000).unwrap();
    let mut client = outgoing
        .handoff(
            TcpTransport::from_stream(client).unwrap(),
            &[],
            material.source.clone(),
            cap,
            cap,
        )
        .unwrap();
    let mut server = incoming
        .handoff(
            TcpTransport::from_stream(server).unwrap(),
            &[],
            material.source.clone(),
            cap,
            cap,
        )
        .unwrap();
    let mut done = false;
    for _ in 0..100_000 {
        if matches!(
            (
                client.handshake(cap).unwrap(),
                server.handshake(cap).unwrap()
            ),
            (Handshake::Complete(_), Handshake::Complete(_))
        ) {
            done = true;
            break;
        }
        std::thread::yield_now();
    }
    assert!(done);
    assert_eq!(client.info().unwrap().peer, PeerVerification::ServerName);
    assert_eq!(pool.available(), 2);
    assert_eq!(set.available(), 0);
    drop(client.into_buffers().unwrap());
    assert_eq!(set.available(), 1);
    drop(server.into_buffers().unwrap());
}
