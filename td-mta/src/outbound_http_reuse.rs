//! Sequential reuse is scoped to one immutable origin, trust store and deadline.
use super::*;

const MAX_REQUESTS: u8 = 32;
const MAX_IDLE: Duration = Duration::from_secs(4);

struct Retained {
    transport: Connection,
    deadline: Instant,
    completed: Instant,
    requests: u8,
}
impl Retained {
    fn eligible(&self, now: Instant) -> bool {
        self.requests < MAX_REQUESTS
            && now < self.deadline
            && now.saturating_duration_since(self.completed) < MAX_IDLE
    }
}

/// At most one connection, 32 sequential requests and 60 seconds per connection.
/// No pipelining or automatic replay. Each response sink remains provisional
/// until success; dropping this owner closes any retained connection.
pub struct ReusableClient<'a> {
    client: Client<'a>,
    retained: Option<Retained>,
}
impl<'a> ReusableClient<'a> {
    pub fn new(client: Client<'a>) -> Result<Self, Error> {
        HttpsUri::parse(client.origin).map_err(|_| Error::Invalid)?;
        pause_check(client.deadline)?;
        Ok(Self {
            client,
            retained: None,
        })
    }

    /// Refuse non-200 responses before calling the sink. Any error discards the
    /// connection. A caller may retry a known read operation with a fresh sink.
    pub fn exchange_success(
        &mut self,
        request: Request<'_>,
        mut sink: impl FnMut(&[u8]) -> Result<(), Error>,
    ) -> Result<Response, Error> {
        let retained = self.retained.take();
        let uri = validate(self.client.origin, &request)?;
        pause_check(self.client.deadline)?;
        let now = Instant::now();
        let retained = retained
            .filter(|value| value.eligible(now))
            .and_then(|mut value| {
                value
                    .transport
                    .idle_boundary()
                    .unwrap_or(false)
                    .then_some(value)
            });
        let mut connection = match retained {
            Some(connection) => connection,
            _ => {
                let deadline = self.client.deadline.min(now + Duration::from_secs(60));
                Retained {
                    transport: connect(&self.client, uri, deadline)?,
                    deadline,
                    completed: now,
                    requests: 0,
                }
            }
        };
        let (response, reusable) = exchange_on(
            &mut connection.transport,
            uri,
            request,
            connection.deadline,
            true,
            Some(200),
            &mut sink,
        )?;
        pause_check(connection.deadline)?;
        connection.requests = connection.requests.checked_add(1).ok_or(Error::Capacity)?;
        connection.completed = Instant::now();
        if reusable && connection.eligible(connection.completed) {
            self.retained = Some(connection);
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::net::TcpListener;
    use td_crypto::{IdentitySelection, ServerConfig};

    fn request() -> Request<'static> {
        Request {
            method: "GET",
            url: "https://localhost/mail",
            body: &[],
            content_type: "application/octet-stream",
            accept: "application/octet-stream",
            authorization: Some("Basic fixture"),
            body_limit: 5,
        }
    }

    fn peer(responses: &[&[u8]], run: impl FnOnce(&mut ReusableClient<'_>)) {
        let (identity, ca) = crate::tls_io::tests::identity_material();
        let trust = TrustStore::from_pem(&ca).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        drop(listener); // Only one TCP connection can serve all requests.
        let deadline = Instant::now() + Duration::from_secs(60);
        std::thread::scope(|scope| {
            let worker = scope.spawn(move || {
                let clock: Arc<dyn Clock> = Arc::new(RuntimeClock::new());
                let cap = Deadline::after(clock.sample().unwrap().monotonic, 60_000).unwrap();
                let config = Arc::new(
                    ServerConfig::new(
                        &[identity],
                        TlsProtocol::Http1,
                        IdentitySelection::RequiredName,
                        None,
                        Arc::new(ClockHandle::new(TlsClockSource::new(clock.clone()))),
                    )
                    .unwrap(),
                );
                let mut tls = TlsIo::new(
                    TcpTransport::from_stream(server).unwrap(),
                    TlsSession::server(config).unwrap(),
                    clock,
                    cap,
                    cap,
                    Box::new([0; TLS_WIRE_BYTES]),
                    Box::new([0; TLS_WIRE_BYTES]),
                )
                .unwrap();
                while tls.handshake().unwrap().is_none() {
                    if !tls.wait_handshake_input(deadline).unwrap() {
                        pause(deadline).unwrap();
                    }
                }
                for response in responses {
                    {
                        let mut reader = BufReader::new(Wire {
                            transport: &mut tls,
                            deadline,
                        });
                        assert_eq!(line(&mut reader, MAX_HEAD).unwrap(), b"GET /mail HTTP/1.1");
                        let mut keep = false;
                        let mut auth = false;
                        loop {
                            let header = line(&mut reader, MAX_HEAD).unwrap();
                            if header.is_empty() {
                                break;
                            }
                            keep |= header == b"Connection: keep-alive";
                            auth |= header == b"Authorization: Basic fixture";
                        }
                        assert!(keep && auth);
                        assert!(reader.buffer().is_empty());
                    }
                    let mut bytes = *response;
                    while !bytes.is_empty() {
                        match tls.write(bytes) {
                            Ok(IoProgress::Bytes(n)) if n > 0 => bytes = bytes.get(n..).unwrap(),
                            Ok(IoProgress::Pending) => pause(deadline).unwrap(),
                            Err(_) => return, // A refusing client may already have closed.
                            other => panic!("unexpected write: {other:?}"),
                        }
                    }
                    loop {
                        match tls.flush() {
                            Ok(FlushProgress::Complete) => break,
                            Ok(FlushProgress::Pending) => pause(deadline).unwrap(),
                            Err(_) => return, // Header refusal does not drain an error body.
                        }
                    }
                }
                // Keep the final response's TLS peer alive until the client is done.
                while tls.read(&mut [0]).is_ok_and(|p| p == IoProgress::Pending) {
                    if !tls.wait_input(deadline).unwrap() {
                        pause(deadline).unwrap();
                    }
                }
            });
            let transport = connected(socket, &trust, "localhost", deadline).unwrap();
            let mut client = ReusableClient::new(Client {
                executable: Path::new("/no-resolver-in-this-test"),
                origin: "https://localhost",
                trust: &trust,
                deadline,
            })
            .unwrap();
            client.retained = Some(Retained {
                transport,
                deadline,
                completed: Instant::now(),
                requests: 0,
            });
            run(&mut client);
            drop(client);
            worker.join().unwrap();
        });
    }

    #[test]
    fn thirty_two_requests_share_one_socket_then_retire() {
        let responses = (0..32).map(|index| {
            if index % 2 == 0 {
                format!("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n{index:05}").into_bytes()
            } else {
                format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{index:05}\r\n0\r\n\r\n").into_bytes()
            }
        }).collect::<Vec<_>>();
        let responses = responses.iter().map(Vec::as_slice).collect::<Vec<_>>();
        peer(&responses, |client| {
            for index in 0..32 {
                // Idle-policy boundaries are checked separately with explicit
                // instants; scheduling pauses must not fail this reuse proof.
                client.retained.as_mut().unwrap().completed = client.client.deadline;
                let mut body = Vec::new();
                let response = client
                    .exchange_success(request(), |part| {
                        body.extend_from_slice(part);
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(response.bytes, 5);
                assert_eq!(body, format!("{index:05}").as_bytes());
                assert_eq!(client.retained.is_some(), index < 31);
            }
        });
    }

    #[test]
    fn close_sink_status_and_framing_failures_retire_without_replay() {
        for (wire, failure) in [
            (
                b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 5\r\n\r\nhello"
                    .as_slice(),
                None,
            ),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhelloEXTRA",
                None,
            ),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello",
                Some(Error::Sink),
            ),
            (
                b"HTTP/1.1 429 Busy\r\nContent-Length: 999999\r\n\r\n",
                Some(Error::Status(429)),
            ),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx",
                Some(Error::Protocol),
            ),
        ] {
            peer(&[wire], |client| {
                let mut calls = 0;
                let actual = client.exchange_success(request(), |_| {
                    calls += 1;
                    if failure == Some(Error::Sink) {
                        Err(Error::Sink)
                    } else {
                        Ok(())
                    }
                });
                assert_eq!(actual.err(), failure);
                assert!(client.retained.is_none());
                if matches!(failure, Some(Error::Status(_) | Error::Protocol)) {
                    assert_eq!(calls, 0);
                }
            });
        }
    }

    #[test]
    fn request_validation_retires_without_sending_and_policy_uses_fixed_caps() {
        peer(&[], |client| {
            let retained = client.retained.as_ref().unwrap();
            let base = retained.completed;
            assert!(retained.eligible(base));
            assert!(!retained.eligible(base + MAX_IDLE));
            assert!(!retained.eligible(retained.deadline));
            let mut invalid = request();
            invalid.url = "https://other.example/mail";
            assert_eq!(
                client
                    .exchange_success(invalid, |_| panic!("invalid sink"))
                    .err(),
                Some(Error::Invalid)
            );
            assert!(client.retained.is_none());
        });
    }
    #[test]
    fn stalled_input_returns_deadline_without_an_elapsed_time_assertion() {
        peer(&[], |client| {
            let mut wire = Wire {
                transport: &mut client.retained.as_mut().unwrap().transport,
                deadline: Instant::now() + Duration::from_millis(10),
            };
            assert_eq!(io_error(wire.read(&mut [0]).unwrap_err()), Error::Deadline);
        });
    }
}
