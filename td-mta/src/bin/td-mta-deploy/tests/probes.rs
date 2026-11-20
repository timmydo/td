#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use super::*;
use std::net::TcpListener;
use td_crypto::{IdentitySelection, ServerConfig, ServerIdentity};
use td_mta::ports::{Error, Time};

use crate::tls_fixture::ServerPems;

const FAKE_NOW: i64 = 1_800_000_000_000;
const TEST_LIMIT: Duration = Duration::from_secs(6);

struct FixtureClock(RuntimeClock);
impl Clock for FixtureClock {
    fn sample(&self) -> Result<Time, Error> {
        Ok(Time {
            monotonic: self.0.monotonic()?,
            utc_ms: FAKE_NOW,
        })
    }
}

struct Fixture {
    clock: Arc<dyn Clock>,
    roots: TrustStore,
    identity: Arc<ServerIdentity>,
}

impl Fixture {
    fn new() -> R<Self> {
        let pems = ServerPems::new(DOMAIN)?;
        let identity = ServerIdentity::from_pem(
            &pems.chain,
            &pems.key,
            &[DOMAIN],
            Some((FAKE_NOW / 1000) as u64),
        )?;
        Ok(Self {
            clock: Arc::new(FixtureClock(RuntimeClock::new())),
            roots: TrustStore::from_pem(&pems.root)?,
            identity: Arc::new(identity),
        })
    }

    fn exchange(
        &self,
        protocol: TlsProtocol,
        reply: &'static [u8],
        trusted: bool,
    ) -> R<(R<()>, R<()>)> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let address = listener.local_addr()?;
        let public = TrustStore::public_roots()?;
        let client = Listener::new(
            protocol,
            address,
            if trusted { &self.roots } else { &public },
            self.clock.clone(),
        )?;
        let clock = self.clock.clone();
        let config = Arc::new(ServerConfig::new(
            std::slice::from_ref(&self.identity),
            protocol,
            if protocol == TlsProtocol::Smtp {
                IdentitySelection::MatchPresentName
            } else {
                IdentitySelection::RequiredName
            },
            None,
            Arc::new(ClockHandle::new(TlsClockSource::new(clock.clone()))),
        )?);
        let server = thread::spawn(move || -> R<()> {
            let mut tcp = TcpTransport::from_stream(listener.accept()?.0)?;
            let end = Instant::now() + TEST_LIMIT;
            let deadline = Deadline::after(clock.monotonic()?, TEST_LIMIT.as_millis() as u64)?;
            if protocol == TlsProtocol::Smtp {
                write(&mut tcp, b"220 fixture ESMTP\r\n", end)?;
                expect_line(&mut tcp, b"EHLO probe.invalid\r\n", end)?;
                write(&mut tcp, b"250-fixture\r\n250 STARTTLS\r\n", end)?;
                expect_line(&mut tcp, b"STARTTLS\r\n", end)?;
                write(&mut tcp, b"220 Ready for TLS\r\n", end)?;
            }
            let mut input = [0; TLS_WIRE_BYTES];
            let mut output = [0; TLS_WIRE_BYTES];
            let mut tls = TlsIo::new(
                tcp,
                TlsSession::server(config)?,
                clock,
                deadline,
                deadline,
                &mut input,
                &mut output,
            )
            .map_err(|_| "server TLS setup")?;
            while tls.handshake()?.is_none() {
                pause(end)?;
            }
            if protocol == TlsProtocol::Smtp {
                expect_line(&mut tls, b"EHLO probe.invalid\r\n", end)?;
                write(&mut tls, reply, end)?;
                if reply.starts_with(b"250 ") {
                    expect_line(&mut tls, b"QUIT\r\n", end)?;
                    write(&mut tls, b"221 Goodbye\r\n", end)?;
                }
            } else {
                for header in [
                    b"GET /.well-known/jmap HTTP/1.1\r\n".as_slice(),
                    b"Host: mx.timmydouglas.com\r\n",
                    b"Connection: close\r\n",
                    b"\r\n",
                ] {
                    expect_line(&mut tls, header, end)?;
                }
                write(&mut tls, reply, end)?;
            }
            Ok(())
        });
        let result = client.probe(TEST_LIMIT);
        Ok((
            result,
            server.join().map_err(|_| "listener fixture panicked")?,
        ))
    }
}

fn expect_line(transport: &mut impl Transport, expected: &[u8], end: Instant) -> R<()> {
    let mut buffer = [0; 1024];
    let n = line(transport, end, &mut buffer)?;
    require(
        &buffer[..n] == expected,
        "unexpected listener probe request",
    )
}

#[test]
fn authenticated_starttls_requires_a_second_ehlo() -> R<()> {
    let fixture = Fixture::new()?;
    let (client, server) = fixture.exchange(TlsProtocol::Smtp, b"250 fixture\r\n", true)?;
    client?;
    server?;
    let (client, server) = fixture.exchange(TlsProtocol::Smtp, b"421 unavailable\r\n", true)?;
    assert!(client.is_err());
    server
}

#[test]
fn jmap_requires_the_authentication_challenge_over_verified_tls() -> R<()> {
    let fixture = Fixture::new()?;
    for (reply, success) in [
        (b"HTTP/1.1 401 Unauthorized\r\n".as_slice(), true),
        (b"HTTP/1.1 200 OK\r\n", false),
        (b"garbage 401\r\n", false),
    ] {
        let (client, server) = fixture.exchange(TlsProtocol::Http1, reply, true)?;
        server?;
        assert_eq!(client.is_ok(), success, "{client:?}");
    }
    let (client, server) =
        fixture.exchange(TlsProtocol::Http1, b"HTTP/1.1 401 Unauthorized\r\n", false)?;
    assert!(client.is_err() && server.is_err());
    Ok(())
}

#[test]
fn stalled_smtp_greeting_exhausts_the_probe_budget() -> R<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let f = Fixture::new()?;
    let client = Listener::new(TlsProtocol::Smtp, listener.local_addr()?, &f.roots, f.clock)?;
    // The kernel accepts the connection but no peer ever sends its greeting.
    let start = Instant::now();
    let result = client.probe(Duration::from_millis(25));
    require(
        result
            .err()
            .is_some_and(|e| e.to_string() == "listener probe timeout"),
        "missing probe timeout",
    )?;
    assert!(start.elapsed() < TEST_LIMIT);
    Ok(())
}
