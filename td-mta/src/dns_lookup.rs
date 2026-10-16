//! Bounded address lookup for one caller-admitted configured hostname.
use crate::{
    dns_reply::{Reply, MAX_ADDRESSES, MAX_ALIASES},
    dns_transport::{self, Budget},
    dns_wire::{AddressType, Name, Question},
    ports::{Clock, Entropy, Error, Resolution, Tick},
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use td_mime::work::{Charge, Stop};

/// Run on the fixed resolver worker. Servers and host must come from the
/// admitted configuration. The host remains the TLS identity after lookup.
/// Output is published only on success; no cache or negative TTL is inferred.
/// Each family uses the supplied alias allowance independently. The caller's
/// allowance is restored on return; this operation completes both chains.
pub fn lookup(
    host: &str,
    servers: &[SocketAddr],
    entropy: &mut dyn Entropy,
    budget: &mut Budget<'_>,
    packet: &mut [u8],
    output: &mut [IpAddr],
) -> Result<Resolution, Error> {
    if servers.is_empty() || servers.len() > 4 || output.len() < MAX_ADDRESSES {
        return Err(Error::Capacity);
    }
    let name = Name::from_host(host).map_err(|_| Error::Invalid)?;
    let allowance = budget.remaining_aliases;
    let result = collect(&name, allowance, |name, kind, remaining| {
        budget.remaining_aliases = remaining;
        let mut last = Error::Dns;
        for server in servers {
            check(budget)?;
            let mut id = [0; 2];
            entropy.fill(&mut id).map_err(|_| Error::Entropy)?;
            check(budget)?;
            let question = Question::from_name(name.clone(), kind, u16::from_be_bytes(id));
            match dns_transport::exchange(*server, &question, budget, packet) {
                Ok(Reply::ResponseCode(3)) => {
                    return Ok((Reply::NoData, check(budget)?));
                }
                Ok(Reply::ResponseCode(_)) | Ok(Reply::Truncated) => last = Error::Dns,
                Ok(reply) => return Ok((reply, check(budget)?)),
                Err(error @ (Error::Capacity | Error::Entropy | Error::Invalid)) => {
                    return Err(error);
                }
                Err(error) => {
                    // An idle timeout may try the next server, but the original
                    // total or sticky meter refusal can never be renewed.
                    check(budget)?;
                    last = error;
                }
            }
        }
        Err(last)
    });
    budget.remaining_aliases = allowance;
    let result = result?;
    check(budget)?;
    output
        .get_mut(..result.count)
        .ok_or(Error::Capacity)?
        .copy_from_slice(result.addresses.get(..result.count).ok_or(Error::Invalid)?);
    Ok(Resolution {
        count: result.count,
        valid_until: result.until,
    })
}

fn check(budget: &mut Budget<'_>) -> Result<Tick, Error> {
    let now = budget.clock.sample()?.monotonic;
    if budget.deadline.expired(now) {
        return Err(Error::Deadline);
    }
    budget
        .meter
        .charge(
            now,
            Charge {
                records: 1,
                ..Charge::default()
            },
        )
        .map_err(|e| match e {
            Stop::Deadline => Error::Deadline,
            _ => Error::Capacity,
        })?;
    Ok(now)
}

struct ResultSet {
    addresses: [IpAddr; MAX_ADDRESSES],
    count: usize,
    until: Tick,
}

fn collect(
    original: &Name,
    allowance: usize,
    mut query: impl FnMut(&Name, AddressType, usize) -> Result<(Reply, Tick), Error>,
) -> Result<ResultSet, Error> {
    if allowance > MAX_ALIASES {
        return Err(Error::Capacity);
    }
    let mut result = ResultSet {
        addresses: [IpAddr::V4(Ipv4Addr::UNSPECIFIED); MAX_ADDRESSES],
        count: 0,
        until: Tick(u64::MAX),
    };
    for kind in [AddressType::A, AddressType::Aaaa] {
        let mut name = original.clone();
        let mut remaining = allowance;
        let mut until = Tick(u64::MAX);
        let mut seen: [Option<Name>; MAX_ALIASES + 1] = std::array::from_fn(|_| None);
        let mut seen_count = 0usize;
        loop {
            let (reply, received) = query(&name, kind, remaining)?;
            if let Some(path) = reply.path() {
                for (index, visited) in path.names().enumerate() {
                    // A continuation starts at the previous packet's last name.
                    if index == 0 && seen_count != 0 {
                        if seen.get(seen_count - 1).and_then(Option::as_ref) != Some(visited) {
                            return Err(Error::Dns);
                        }
                        continue;
                    }
                    if seen.iter().flatten().any(|old| old == visited) {
                        return Err(Error::Dns);
                    }
                    *seen.get_mut(seen_count).ok_or(Error::Dns)? = Some(visited.clone());
                    seen_count += 1;
                }
            }
            match reply {
                Reply::Alias {
                    name: next,
                    hops,
                    ttl,
                    ..
                } => {
                    if hops == 0 {
                        return Err(Error::Dns);
                    }
                    remaining = remaining.checked_sub(hops).ok_or(Error::Dns)?;
                    until = until.min(expiry(received, ttl)?);
                    name = next;
                }
                Reply::Addresses(addresses) => {
                    until = until.min(expiry(received, addresses.ttl())?);
                    for address in addresses.values() {
                        if result
                            .addresses
                            .get(..result.count)
                            .ok_or(Error::Invalid)?
                            .contains(address)
                        {
                            continue;
                        }
                        *result
                            .addresses
                            .get_mut(result.count)
                            .ok_or(Error::Capacity)? = *address;
                        result.count += 1;
                    }
                    result.until = result.until.min(until);
                    break;
                }
                Reply::NoData => {
                    // No SOA negative TTL is decoded. Do not cache absence of
                    // either family as part of an otherwise positive set.
                    result.until = result.until.min(received);
                    break;
                }
                Reply::Truncated | Reply::ResponseCode(_) => return Err(Error::Dns),
            }
        }
    }
    if result.count == 0 {
        return Err(Error::Dns);
    }
    Ok(result)
}

fn expiry(received: Tick, ttl: u32) -> Result<Tick, Error> {
    received
        .0
        .checked_add(u64::from(ttl) * 1000)
        .map(Tick)
        .ok_or(Error::Invalid)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::{clock::RuntimeClock, dns_reply, dns_wire::MAX_PACKET_BYTES, ports::Deadline};
    use std::{net::UdpSocket, thread, time::Duration};
    use td_mime::work::Meter;

    fn name(value: &str) -> Name {
        Name::from_host(value).unwrap()
    }
    fn answer(query: &[u8], ttl: u32, count: u8) -> Vec<u8> {
        let mut bytes = query.to_vec();
        bytes[2] |= 0x80;
        bytes[7] = count;
        let kind = query[query.len() - 3];
        for index in 0..count {
            bytes.extend_from_slice(&[0xc0, 12, 0, kind, 0, 1]);
            bytes.extend_from_slice(&ttl.to_be_bytes());
            if kind == 1 {
                bytes.extend_from_slice(&[0, 4, 192, 0, 2, index]);
            } else {
                bytes.extend_from_slice(&[0, 16]);
                bytes.extend_from_slice(&[
                    0x20, 1, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, index,
                ]);
            }
        }
        bytes
    }
    fn addresses(name: &Name, kind: AddressType, ttl: u32, count: u8) -> Reply {
        let query = Question::from_name(name.clone(), kind, 123);
        let mut cell = [0; 271];
        let bytes = answer(query.write(&mut cell).unwrap(), ttl, count);
        dns_reply::read(&bytes, &query, MAX_ALIASES).unwrap()
    }
    #[test]
    fn both_families_keep_each_received_ttl_and_original_name() {
        let original = name("relay.test");
        let alias = name("alias.test");
        let mut turn = 0;
        let result = collect(&original, MAX_ALIASES, |current, kind, remaining| {
            turn += 1;
            match turn {
                1 => {
                    assert_eq!(current, &original);
                    assert_eq!(kind, AddressType::A);
                    Ok((
                        chain(current, kind, std::slice::from_ref(&alias), false, 1),
                        Tick(1000),
                    ))
                }
                2 => {
                    assert_eq!(current, &alias);
                    assert_eq!(remaining, 7);
                    Ok((addresses(current, kind, 60, 2), Tick(1900)))
                }
                3 => {
                    assert_eq!(current, &original);
                    assert_eq!(kind, AddressType::Aaaa);
                    assert_eq!(remaining, 8);
                    Ok((addresses(current, kind, 60, 1), Tick(2000)))
                }
                _ => panic!("extra query"),
            }
        })
        .unwrap();
        assert_eq!(turn, 3);
        assert_eq!(result.count, 3);
        assert_eq!(result.until, Tick(2000));
        assert!(result.addresses[0].is_ipv4());
        assert!(result.addresses[2].is_ipv6());
    }
    #[test]
    fn missing_family_and_zero_ttl_are_current_lookup_only() {
        let original = name("relay.test");
        for missing in [false, true] {
            let result = collect(&original, 8, |current, kind, _| {
                Ok((
                    if kind == AddressType::Aaaa && missing {
                        Reply::NoData
                    } else {
                        addresses(current, kind, if missing { 60 } else { 0 }, 1)
                    },
                    Tick(500),
                ))
            })
            .unwrap();
            assert_eq!(result.until, Tick(500));
            assert_eq!(result.count, if missing { 1 } else { 2 });
        }
        assert!(matches!(
            collect(&original, 8, |_, _, _| Ok((Reply::NoData, Tick(0)))),
            Err(Error::Dns)
        ));
        assert_eq!(expiry(Tick(u64::MAX), 1), Err(Error::Invalid));
    }
    #[test]
    fn aggregate_capacity_and_cross_packet_hops_refuse() {
        let original = name("relay.test");
        assert!(matches!(
            collect(&original, 8, |current, kind, _| {
                Ok((
                    addresses(
                        current,
                        kind,
                        60,
                        if kind == AddressType::A { 16 } else { 1 },
                    ),
                    Tick(0),
                ))
            }),
            Err(Error::Capacity)
        ));
        for hops in [8, 9] {
            let mut count = 0;
            let result = collect(&original, 8, |current, kind, remaining| {
                if kind == AddressType::Aaaa {
                    return Ok((Reply::NoData, Tick(0)));
                }
                if count < hops {
                    assert_eq!(remaining, 8 - count);
                    count += 1;
                    Ok((
                        chain(
                            current,
                            kind,
                            &[name(&format!("alias{count}.test"))],
                            false,
                            60,
                        ),
                        Tick(0),
                    ))
                } else {
                    assert_eq!(remaining, 0);
                    Ok((addresses(current, kind, 60, 1), Tick(0)))
                }
            });
            assert_eq!(result.is_ok(), hops == 8);
        }
        let mut count = 0;
        assert!(matches!(
            collect(&original, 8, |current, kind, _| {
                count += 1;
                Ok((
                    chain(
                        current,
                        kind,
                        &[if count == 1 {
                            name("alias.test")
                        } else {
                            original.clone()
                        }],
                        false,
                        1,
                    ),
                    Tick(0),
                ))
            }),
            Err(Error::Dns)
        ));
        assert_eq!(count, 2);
    }
    struct Random(u16);
    impl Entropy for Random {
        fn fill(&mut self, output: &mut [u8]) -> Result<(), crate::ports::CryptoError> {
            self.0 += 1;
            output.copy_from_slice(&self.0.to_be_bytes());
            Ok(())
        }
    }
    fn capacity() -> Charge {
        Charge {
            records: 100_000,
            io_bytes: 8 * 1024 * 1024,
            output_bytes: 1024 * 1024,
            unlinks: 0,
        }
    }
    #[test]
    fn numeric_servers_fall_back_with_fresh_ids_and_atomic_output() {
        let servers: Vec<_> = (0..2)
            .map(|_| {
                let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                socket
            })
            .collect();
        let endpoints: Vec<_> = servers.iter().map(|s| s.local_addr().unwrap()).collect();
        thread::scope(|scope| {
            for (index, server) in servers.iter().enumerate() {
                scope.spawn(move || {
                    for family in 0..2 {
                        let mut query = [0; 512];
                        let (size, peer) = server.recv_from(&mut query).unwrap();
                        assert_eq!(
                            u16::from_be_bytes([query[0], query[1]]),
                            (family * 2 + index + 1) as u16
                        );
                        let mut response =
                            answer(&query[..size], 60, if index == 0 { 0 } else { 1 });
                        if index == 0 {
                            response[3] |= 2;
                        }
                        server.send_to(&response, peer).unwrap();
                    }
                });
            }
            let clock = RuntimeClock::new();
            let deadline = Deadline::after(clock.sample().unwrap().monotonic, 2000).unwrap();
            let mut meter = Meter::new(deadline, capacity());
            let mut budget = Budget {
                clock: &clock,
                deadline,
                meter: &mut meter,
                remaining_aliases: 8,
            };
            let mut packet = vec![0; MAX_PACKET_BYTES];
            let sentinel = IpAddr::from([203, 0, 113, 9]);
            let mut output = [sentinel; MAX_ADDRESSES];
            let mut random = Random(0);
            let result = lookup(
                "relay.test",
                &endpoints,
                &mut random,
                &mut budget,
                &mut packet,
                &mut output,
            )
            .unwrap();
            assert_eq!(result.count, 2);
            assert!(output[0].is_ipv4());
            assert!(output[1].is_ipv6());
            assert!(output[2..].iter().all(|a| *a == sentinel));
            assert_eq!(random.0, 4);
            assert_eq!(budget.remaining_aliases, 8);
            let before = output;
            assert_eq!(
                lookup(
                    "relay.test",
                    &[],
                    &mut random,
                    &mut budget,
                    &mut packet,
                    &mut output
                ),
                Err(Error::Capacity)
            );
            assert_eq!(output, before);
        });
    }
    #[test]
    fn second_family_failure_does_not_publish_first_family() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let endpoint = server.local_addr().unwrap();
        thread::scope(|scope| {
            scope.spawn(|| {
                for family in 0..2 {
                    let mut query = [0; 512];
                    let (size, peer) = server.recv_from(&mut query).unwrap();
                    let mut response = answer(&query[..size], 60, if family == 0 { 1 } else { 0 });
                    if family == 1 {
                        response[3] |= 2;
                    }
                    server.send_to(&response, peer).unwrap();
                }
            });
            let clock = RuntimeClock::new();
            let deadline = Deadline::after(clock.sample().unwrap().monotonic, 2000).unwrap();
            let mut meter = Meter::new(deadline, capacity());
            let mut budget = Budget {
                clock: &clock,
                deadline,
                meter: &mut meter,
                remaining_aliases: 8,
            };
            let mut packet = vec![0; MAX_PACKET_BYTES];
            let before = [IpAddr::from([203, 0, 113, 9]); MAX_ADDRESSES];
            let mut output = before;
            assert_eq!(
                lookup(
                    "relay.test",
                    &[endpoint],
                    &mut Random(0),
                    &mut budget,
                    &mut packet,
                    &mut output
                ),
                Err(Error::Dns)
            );
            assert_eq!(output, before);
            assert_eq!(budget.remaining_aliases, 8);
        });
    }
    fn chain(
        current: &Name,
        kind: AddressType,
        targets: &[Name],
        terminal: bool,
        ttl: u32,
    ) -> Reply {
        let q = Question::from_name(current.clone(), kind, 123);
        let mut cell = [0; 271];
        let mut bytes = q.write(&mut cell).unwrap().to_vec();
        bytes[2] |= 0x80;
        bytes[7] = (targets.len() + usize::from(terminal)) as u8;
        let mut owner = current;
        for target in targets {
            bytes.extend_from_slice(owner.wire());
            bytes.extend_from_slice(&[0, 5, 0, 1]);
            bytes.extend_from_slice(&ttl.to_be_bytes());
            bytes.extend_from_slice(&(target.wire().len() as u16).to_be_bytes());
            bytes.extend_from_slice(target.wire());
            owner = target;
        }
        if terminal {
            assert_eq!(kind, AddressType::A);
            bytes.extend_from_slice(owner.wire());
            bytes.extend_from_slice(&[0, 1, 0, 1]);
            bytes.extend_from_slice(&ttl.to_be_bytes());
            bytes.extend_from_slice(&[0, 4, 192, 0, 2, 1]);
        }
        dns_reply::read(&bytes, &q, MAX_ALIASES).unwrap()
    }
    #[test]
    fn cross_packet_cycles_include_terminal_and_hidden_intermediate_names() {
        let original = name("relay.test");
        for hidden in [false, true] {
            let mut calls = 0;
            let result = collect(&original, 8, |current, kind, _| {
                if kind == AddressType::Aaaa {
                    return Ok((Reply::NoData, Tick(0)));
                }
                calls += 1;
                let targets = if calls == 1 {
                    if hidden {
                        vec![name("hidden.test"), name("alias.test")]
                    } else {
                        vec![name("alias.test")]
                    }
                } else if hidden {
                    vec![name("hidden.test"), name("end.test")]
                } else {
                    vec![original.clone()]
                };
                Ok((chain(current, kind, &targets, calls == 2, 60), Tick(0)))
            });
            assert!(matches!(result, Err(Error::Dns)), "hidden={hidden}");
            assert_eq!(calls, 2);
        }
    }
}
