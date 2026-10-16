//! One deadline-owned DNS exchange on the fixed resolver worker.
use crate::{
    clock::RuntimeClock,
    dns_reply::{self, Reply, MAX_ALIASES, MAX_RECORDS},
    dns_wire::{Question, MAX_PACKET_BYTES, MAX_QUERY_BYTES},
    ports::{Clock, Deadline, Error, Tick},
};
use std::{
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, UdpSocket},
    time::{Duration, Instant},
};
use td_mime::work::{Charge, Meter, Stop};

pub const PACKET_BYTES: usize = MAX_PACKET_BYTES;
pub const MAX_DATAGRAMS: usize = 16;
const UDP_BYTES: usize = 512;
const IO_BYTES: usize = 16 * 1024;
const IDLE: Duration = Duration::from_secs(1);

/// Original clock/meter domain, shared across UDP, fallback and short I/O.
pub struct Budget<'a> {
    pub clock: &'a RuntimeClock,
    pub deadline: Deadline,
    pub meter: &'a mut Meter,
    /// Subtract returned Alias hops before continuing the original chain.
    pub remaining_aliases: usize,
}
impl Budget<'_> {
    fn check(&mut self) -> Result<Duration, Error> {
        let now = self.clock.sample()?.monotonic;
        if self.deadline.expired(now) {
            return Err(Error::Deadline);
        }
        self.meter.charge(now, Charge::default()).map_err(work)?;
        let tick = self.deadline.tick().0.min(self.meter.deadline().tick().0);
        let until = self
            .clock
            .deadline_instant(Deadline::after(Tick(0), tick)?)?;
        let remaining = until
            .checked_duration_since(Instant::now())
            .ok_or(Error::Deadline)?;
        if remaining.is_zero() {
            return Err(Error::Deadline);
        }
        Ok(remaining.min(IDLE))
    }
    fn charge(&mut self, charge: Charge) -> Result<(), Error> {
        self.check()?;
        self.meter
            .charge(self.clock.sample()?.monotonic, charge)
            .map_err(work)
    }
    fn io(&mut self, bytes: usize) -> Result<(), Error> {
        self.charge(Charge {
            records: 1,
            io_bytes: bytes as u64,
            ..Charge::default()
        })
    }
    fn parse(
        &mut self,
        packet: &[u8],
        question: &Question,
    ) -> Result<Result<Reply, crate::dns_wire::Error>, Error> {
        self.charge(Charge {
            // One framing pass plus at most nine answer scans.
            records: ((MAX_ALIASES + 2) * MAX_RECORDS) as u64,
            ..Charge::default()
        })?;
        let result = dns_reply::read(packet, question, self.remaining_aliases);
        self.check()?;
        Ok(result)
    }
}
fn work(error: Stop) -> Error {
    match error {
        Stop::Deadline => Error::Deadline,
        _ => Error::Capacity,
    }
}
fn io(error: std::io::Error) -> Error {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => Error::Deadline,
        _ => error.into(),
    }
}

/// The caller admits the configured numeric resolver and supplies a question
/// with an unpredictable ID. No ambient resolver, cache or TLS identity change.
/// Packet storage is reused; an error leaves arbitrary untrusted response bytes.
/// Blocking kernel calls retain the runtime's documented host-failure boundary.
pub fn exchange(
    endpoint: SocketAddr,
    question: &Question,
    budget: &mut Budget<'_>,
    packet: &mut [u8],
) -> Result<Reply, Error> {
    budget.check()?;
    if budget.remaining_aliases > MAX_ALIASES {
        return Err(Error::Capacity);
    }
    let packet = packet.get_mut(..PACKET_BYTES).ok_or(Error::Capacity)?;
    let mut query = [0; MAX_QUERY_BYTES];
    let query = question.write(&mut query).map_err(|_| Error::Invalid)?;
    let local = SocketAddr::new(
        match endpoint.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        },
        0,
    );
    budget.io(0)?;
    let result = UdpSocket::bind(local);
    budget.check()?;
    let socket = result.map_err(io)?;
    budget.io(0)?;
    let result = socket.connect(endpoint);
    budget.check()?;
    result.map_err(io)?;
    socket
        .set_write_timeout(Some(budget.check()?))
        .map_err(io)?;
    budget.charge(Charge {
        output_bytes: query.len() as u64,
        ..Charge::default()
    })?;
    budget.io(query.len())?;
    let result = socket.send(query);
    budget.check()?;
    if result.map_err(io)? != query.len() {
        return Err(Error::Dns);
    }
    for _ in 0..MAX_DATAGRAMS {
        socket.set_read_timeout(Some(budget.check()?)).map_err(io)?;
        budget.io(UDP_BYTES + 1)?;
        let result = socket.recv(packet.get_mut(..UDP_BYTES + 1).ok_or(Error::Capacity)?);
        budget.check()?;
        let size = result.map_err(io)?;
        if size > UDP_BYTES {
            return Err(Error::Dns);
        }
        let data = packet.get(..size).ok_or(Error::Invalid)?;
        match budget.parse(data, question)? {
            Ok(Reply::Truncated) => {
                drop(socket);
                return tcp(endpoint, question, query, budget, packet);
            }
            Ok(reply) => return Ok(reply),
            Err(_) => continue,
        }
    }
    Err(Error::Dns)
}
fn tcp(
    endpoint: SocketAddr,
    question: &Question,
    query: &[u8],
    budget: &mut Budget<'_>,
    packet: &mut [u8],
) -> Result<Reply, Error> {
    let timeout = budget.check()?;
    budget.io(0)?;
    let result = TcpStream::connect_timeout(&endpoint, timeout);
    budget.check()?;
    let mut socket = result.map_err(io)?;
    let mut frame = [0; MAX_QUERY_BYTES + 2];
    frame.get_mut(..2).ok_or(Error::Capacity)?.copy_from_slice(
        &u16::try_from(query.len())
            .map_err(|_| Error::Invalid)?
            .to_be_bytes(),
    );
    let end = query.len() + 2;
    frame
        .get_mut(2..end)
        .ok_or(Error::Capacity)?
        .copy_from_slice(query);
    budget.charge(Charge {
        output_bytes: end as u64,
        ..Charge::default()
    })?;
    let mut offset = 0usize;
    while offset < end {
        socket
            .set_write_timeout(Some(budget.check()?))
            .map_err(io)?;
        let bytes = frame.get(offset..end).ok_or(Error::Invalid)?;
        budget.io(bytes.len())?;
        let result = socket.write(bytes);
        budget.check()?;
        let size = result.map_err(io)?;
        if size == 0 || size > bytes.len() {
            return Err(Error::Dns);
        }
        offset += size;
    }
    let mut prefix = [0; 2];
    receive(&mut socket, &mut prefix, budget)?;
    let size = usize::from(u16::from_be_bytes(prefix));
    if size < 12 {
        return Err(Error::Dns);
    }
    let packet = packet.get_mut(..size).ok_or(Error::Capacity)?;
    receive(&mut socket, packet, budget)?;
    match budget.parse(packet, question)? {
        Ok(Reply::Truncated) | Err(_) => Err(Error::Dns),
        Ok(reply) => Ok(reply),
    }
}
fn receive(
    socket: &mut TcpStream,
    output: &mut [u8],
    budget: &mut Budget<'_>,
) -> Result<(), Error> {
    let mut offset = 0usize;
    while offset < output.len() {
        socket.set_read_timeout(Some(budget.check()?)).map_err(io)?;
        let end = output.len().min(offset + IO_BYTES);
        let bytes = output.get_mut(offset..end).ok_or(Error::Invalid)?;
        budget.io(bytes.len())?;
        let result = socket.read(bytes);
        budget.check()?;
        let size = result.map_err(io)?;
        if size == 0 || size > bytes.len() {
            return Err(Error::Dns);
        }
        offset += size;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::dns_wire::AddressType;
    use std::{net::TcpListener, thread};
    fn capacity() -> Charge {
        Charge {
            records: 100_000,
            io_bytes: 8 * 1024 * 1024,
            output_bytes: 1024 * 1024,
            unlinks: 0,
        }
    }
    fn server() -> UdpSocket {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        socket
    }
    fn question() -> Question {
        Question::new("relay.test", AddressType::A, 0x1234).unwrap()
    }
    fn response(query: &[u8]) -> Vec<u8> {
        let mut bytes = query.to_vec();
        bytes[2] |= 0x80;
        bytes[7] = 1;
        bytes
            .extend_from_slice(b"\xc0\x0c\x00\x01\x00\x01\x00\x00\x00\x05\x00\x04\xc0\x00\x02\x01");
        bytes
    }
    fn lookup(endpoint: SocketAddr, milliseconds: u64, limit: Charge) -> Result<Reply, Error> {
        lookup_deadlines(endpoint, milliseconds, milliseconds, limit)
    }
    fn lookup_deadlines(
        endpoint: SocketAddr,
        job_ms: u64,
        meter_ms: u64,
        limit: Charge,
    ) -> Result<Reply, Error> {
        let clock = RuntimeClock::new();
        let now = clock.sample().unwrap().monotonic;
        let deadline = Deadline::after(now, job_ms).unwrap();
        let mut meter = Meter::new(Deadline::after(now, meter_ms).unwrap(), limit);
        let mut budget = Budget {
            clock: &clock,
            deadline,
            meter: &mut meter,
            remaining_aliases: MAX_ALIASES,
        };
        exchange(
            endpoint,
            &question(),
            &mut budget,
            &mut vec![0; PACKET_BYTES],
        )
    }
    #[test]
    fn connected_udp_ignores_mismatches_and_returns_exact_addresses() {
        let socket = server();
        let endpoint = socket.local_addr().unwrap();
        thread::scope(|scope| {
            scope.spawn(|| {
                let mut bytes = [0; 512];
                let (count, peer) = socket.recv_from(&mut bytes).unwrap();
                let mut expected = [0; MAX_QUERY_BYTES];
                assert_eq!(&bytes[..count], question().write(&mut expected).unwrap());
                let answer = response(&bytes[..count]);
                let mut wrong = answer.clone();
                wrong[0] ^= 1;
                socket.send_to(&wrong, peer).unwrap();
                socket.send_to(&answer, peer).unwrap();
            });
            let Reply::Addresses(answer) = lookup(endpoint, 2000, capacity()).unwrap() else {
                panic!("addresses");
            };
            assert_eq!(answer.values(), [IpAddr::from([192, 0, 2, 1])]);
            assert_eq!(answer.ttl(), 5);
        });
    }
    fn accept(listener: &TcpListener) -> TcpStream {
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            match listener.accept() {
                Ok((socket, _)) => {
                    socket
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    return socket;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < until => {
                    thread::sleep(Duration::from_millis(1))
                }
                result => panic!("accept: {result:?}"),
            }
        }
    }
    #[test]
    fn udp_truncation_uses_exact_tcp_framing_and_rejects_bad_completion() {
        for mode in 0..4 {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let endpoint = listener.local_addr().unwrap();
            let udp = UdpSocket::bind(endpoint).unwrap();
            udp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            thread::scope(|scope| {
                scope.spawn(|| {
                    let mut bytes = [0; 512];
                    let (count, peer) = udp.recv_from(&mut bytes).unwrap();
                    let query = bytes[..count].to_vec();
                    bytes[2] |= 0x82;
                    udp.send_to(&bytes[..count], peer).unwrap();
                    let mut tcp = accept(&listener);
                    let mut prefix = [0; 2];
                    tcp.read_exact(&mut prefix).unwrap();
                    let size = usize::from(u16::from_be_bytes(prefix));
                    assert_eq!(size, query.len());
                    tcp.read_exact(&mut bytes[..size]).unwrap();
                    assert_eq!(&bytes[..size], query);
                    let mut answer = response(&query);
                    if mode == 1 {
                        answer[2] |= 2;
                    }
                    let length = if mode == 2 { 0 } else { answer.len() as u16 };
                    tcp.write_all(&length.to_be_bytes()).unwrap();
                    if mode == 3 {
                        tcp.write_all(&answer[..answer.len() - 1]).unwrap();
                    } else if mode != 2 {
                        for chunk in answer.chunks(3) {
                            tcp.write_all(chunk).unwrap();
                        }
                    }
                });
                let result = lookup(endpoint, 2000, capacity());
                if mode == 0 {
                    assert!(matches!(result, Ok(Reply::Addresses(_))));
                } else {
                    assert_eq!(result, Err(Error::Dns));
                }
            });
        }
    }
    #[test]
    fn bounded_datagram_refusal_and_pre_io_capacity() {
        for oversized in [false, true] {
            let socket = server();
            let endpoint = socket.local_addr().unwrap();
            thread::scope(|scope| {
                scope.spawn(|| {
                    let mut bytes = [0; 512];
                    let (_, peer) = socket.recv_from(&mut bytes).unwrap();
                    if oversized {
                        socket.send_to(&[0; 513], peer).unwrap();
                    } else {
                        for _ in 0..MAX_DATAGRAMS {
                            socket.send_to(&[0; 12], peer).unwrap();
                        }
                    }
                });
                assert_eq!(lookup(endpoint, 2000, capacity()), Err(Error::Dns));
            });
        }
        let socket = server();
        socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let mut limit = capacity();
        limit.records = 0;
        assert_eq!(
            lookup(socket.local_addr().unwrap(), 2000, limit),
            Err(Error::Capacity)
        );
        assert!(socket.recv_from(&mut [0; 512]).is_err());
        let clock = RuntimeClock::new();
        let deadline = Deadline::after(clock.sample().unwrap().monotonic, 2000).unwrap();
        let mut meter = Meter::new(deadline, capacity());
        let mut budget = Budget {
            clock: &clock,
            deadline,
            meter: &mut meter,
            remaining_aliases: MAX_ALIASES,
        };
        assert_eq!(
            exchange(
                socket.local_addr().unwrap(),
                &question(),
                &mut budget,
                &mut [0; 1]
            ),
            Err(Error::Capacity)
        );
        budget.remaining_aliases = MAX_ALIASES + 1;
        assert_eq!(
            exchange(
                socket.local_addr().unwrap(),
                &question(),
                &mut budget,
                &mut vec![0; PACKET_BYTES]
            ),
            Err(Error::Capacity)
        );
        assert!(socket.recv_from(&mut [0; 512]).is_err());
    }
    #[test]
    fn original_job_and_meter_deadlines_are_not_replaced_by_udp_idle_timeout() {
        for (job_ms, meter_ms) in [(100, 2000), (2000, 100)] {
            let socket = server();
            let endpoint = socket.local_addr().unwrap();
            thread::scope(|scope| {
                scope.spawn(|| {
                    let mut bytes = [0; 512];
                    if let Ok((count, peer)) = socket.recv_from(&mut bytes) {
                        thread::sleep(Duration::from_millis(400));
                        let _ = socket.send_to(&response(&bytes[..count]), peer);
                    }
                });
                assert_eq!(
                    lookup_deadlines(endpoint, job_ms, meter_ms, capacity()),
                    Err(Error::Deadline)
                );
            });
        }
    }
    #[test]
    fn tcp_body_read_retains_original_job_and_meter_deadlines() {
        for (job_ms, meter_ms) in [(200, 2000), (2000, 200)] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let endpoint = listener.local_addr().unwrap();
            let udp = UdpSocket::bind(endpoint).unwrap();
            udp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            thread::scope(|scope| {
                scope.spawn(|| {
                    let mut bytes = [0; 512];
                    let (count, peer) = udp.recv_from(&mut bytes).unwrap();
                    let query = bytes[..count].to_vec();
                    bytes[2] |= 0x82;
                    udp.send_to(&bytes[..count], peer).unwrap();
                    let mut tcp = accept(&listener);
                    let mut prefix = [0; 2];
                    tcp.read_exact(&mut prefix).unwrap();
                    let size = usize::from(u16::from_be_bytes(prefix));
                    tcp.read_exact(&mut bytes[..size]).unwrap();
                    let answer = response(&query);
                    tcp.write_all(&(answer.len() as u16).to_be_bytes()).unwrap();
                    tcp.write_all(&answer[..1]).unwrap();
                    thread::sleep(Duration::from_millis(500));
                    let _ = tcp.write_all(&answer[1..]);
                });
                assert_eq!(
                    lookup_deadlines(endpoint, job_ms, meter_ms, capacity()),
                    Err(Error::Deadline)
                );
            });
        }
    }
}
