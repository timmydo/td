//! Bounded numeric dialing on the admitted outbound handshake worker.
use crate::{
    clock::RuntimeClock,
    dns_reply::MAX_ADDRESSES,
    ports::{Clock, Deadline, Error, Tick},
    transport::TcpTransport,
};
use std::{
    net::{IpAddr, SocketAddr, TcpStream},
    time::{Duration, Instant},
};
use td_mime::work::{Charge, Meter, Stop};

const ATTEMPT: Duration = Duration::from_secs(2);

/// Caller retains the sole outbound slot, configured endpoint/TLS policy and
/// original clock domain. DNS expiry governs cache reuse, not this connection's
/// TLS identity. No TLS, credential or queue authority follows from TCP success.
pub fn connect(
    addresses: &[IpAddr],
    port: u16,
    clock: &RuntimeClock,
    deadline: Deadline,
    meter: &mut Meter,
) -> Result<TcpTransport, Error> {
    timeout(clock, deadline, meter)?;
    if port == 0 || addresses.is_empty() || addresses.len() > MAX_ADDRESSES {
        return Err(Error::Invalid);
    }
    let mut last = Error::Dns;
    for address in addresses {
        let remaining = timeout(clock, deadline, meter)?;
        meter
            .charge(
                clock.sample()?.monotonic,
                Charge {
                    records: 2,
                    ..Charge::default()
                },
            )
            .map_err(work)?;
        let result = TcpStream::connect_timeout(&SocketAddr::new(*address, port), remaining);
        timeout(clock, deadline, meter)?;
        match result {
            Ok(socket) => {
                let result = TcpTransport::from_stream(socket);
                timeout(clock, deadline, meter)?;
                match result {
                    Ok(transport) => return Ok(transport),
                    Err(error) => last = error,
                }
            }
            Err(error) => last = error.into(),
        }
    }
    Err(last)
}

fn timeout(clock: &RuntimeClock, deadline: Deadline, meter: &mut Meter) -> Result<Duration, Error> {
    let now = clock.sample()?.monotonic;
    if deadline.expired(now) {
        return Err(Error::Deadline);
    }
    meter.charge(now, Charge::default()).map_err(work)?;
    let tick = deadline.tick().0.min(meter.deadline().tick().0);
    let until = clock.deadline_instant(Deadline::after(Tick(0), tick)?)?;
    let remaining = until
        .checked_duration_since(Instant::now())
        .ok_or(Error::Deadline)?;
    if remaining.is_zero() {
        return Err(Error::Deadline);
    }
    Ok(remaining.min(ATTEMPT))
}
fn work(error: Stop) -> Error {
    match error {
        Stop::Deadline => Error::Deadline,
        _ => Error::Capacity,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::ports::{IoProgress, Transport};
    use std::{
        io::Read,
        net::{Ipv4Addr, TcpListener},
    };
    fn meter(deadline: Deadline, records: u64) -> Meter {
        Meter::new(
            deadline,
            Charge {
                records,
                ..Charge::default()
            },
        )
    }
    #[test]
    fn refused_address_falls_back_to_one_owned_nonblocking_transport() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = listener.local_addr().unwrap();
        let clock = RuntimeClock::new();
        let deadline = Deadline::after(clock.sample().unwrap().monotonic, 2000).unwrap();
        let mut budget = meter(deadline, 4);
        let addresses = [IpAddr::from([127, 0, 0, 2]), endpoint.ip()];
        let mut connection =
            connect(&addresses, endpoint.port(), &clock, deadline, &mut budget).unwrap();
        assert_eq!(connection.peer_addr(), endpoint);
        assert_eq!(budget.remaining().records, 0);
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        assert_eq!(connection.read(&mut [0; 1]), Ok(IoProgress::Pending));
        drop(connection);
        assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
    }
    #[test]
    fn original_job_meter_and_capacity_refuse_before_connect() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = listener.local_addr().unwrap();
        let clock = RuntimeClock::new();
        let future = Deadline::after(clock.sample().unwrap().monotonic, 2000).unwrap();
        let expired = Deadline::after(Tick(0), 0).unwrap();
        for (job, meter_deadline, records, expected) in [
            (expired, future, 32, Error::Deadline),
            (future, expired, 32, Error::Deadline),
            (future, future, 1, Error::Capacity),
        ] {
            assert_eq!(
                connect(
                    &[endpoint.ip()],
                    endpoint.port(),
                    &clock,
                    job,
                    &mut meter(meter_deadline, records)
                )
                .err(),
                Some(expected)
            );
        }
        for (addresses, port) in [
            (vec![], endpoint.port()),
            (vec![endpoint.ip(); MAX_ADDRESSES + 1], endpoint.port()),
            (vec![endpoint.ip()], 0),
        ] {
            assert_eq!(
                connect(&addresses, port, &clock, future, &mut meter(future, 32)).err(),
                Some(Error::Invalid)
            );
        }
        assert_eq!(
            connect(
                &[IpAddr::from([127, 0, 0, 2]), endpoint.ip()],
                endpoint.port(),
                &clock,
                future,
                &mut meter(future, 2)
            )
            .err(),
            Some(Error::Capacity)
        );
        assert_eq!(
            listener.accept().err().unwrap().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
