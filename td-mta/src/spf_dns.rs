//! DNS for a single direct-MX observation, on its own worker and deadline.
use crate::{
    clock::RuntimeClock,
    config::outbound::MAX_RESOLVERS,
    dns_reply::{self, Message, Path, MAX_ALIASES},
    dns_transport::{self, Budget, Packet},
    dns_wire::{Error as WireError, Name, WireQuestion},
    ports::{Deadline, Entropy, Error},
    spf::{Dns, DnsError, Query, Record, MAX_PTR_NAMES},
};
use std::{io::ErrorKind, net::SocketAddr};
use td_mime::work::{Charge, Meter};

/// Includes alias continuations, retries, malformed replies and TCP fallback.
const WORK: Charge = Charge {
    records: 524_288,
    io_bytes: 4 * 1024 * 1024,
    output_bytes: 512 * 1024,
    unlinks: 0,
};

pub(crate) struct Resolver<'a> {
    servers: &'a [SocketAddr],
    current: &'a dyn Fn() -> bool,
    clock: &'a RuntimeClock,
    deadline: Deadline,
    entropy: &'a mut dyn Entropy,
    packet: &'a mut [u8],
    meter: Meter,
}

impl<'a> Resolver<'a> {
    pub(crate) fn new(
        servers: &'a [SocketAddr],
        clock: &'a RuntimeClock,
        deadline: Deadline,
        entropy: &'a mut dyn Entropy,
        packet: &'a mut [u8],
    ) -> Result<Self, Error> {
        if servers.is_empty() || servers.len() > MAX_RESOLVERS {
            return Err(Error::Invalid);
        }
        let packet = packet
            .get_mut(..dns_transport::PACKET_BYTES)
            .ok_or(Error::Capacity)?;
        Ok(Self {
            servers,
            current: &|| true,
            clock,
            deadline,
            entropy,
            packet,
            meter: Meter::new(deadline, WORK),
        })
    }

    pub(crate) fn while_current(mut self, current: &'a dyn Fn() -> bool) -> Self {
        self.current = current;
        self
    }

    fn query(
        &mut self,
        name: &Name,
        kind: Query,
        remaining: usize,
    ) -> Result<Option<Answer>, DnsError> {
        let mut budget = Budget {
            clock: self.clock,
            deadline: self.deadline,
            meter: &mut self.meter,
            remaining_aliases: remaining,
        };
        for server in self.servers {
            for edns in [true, false] {
                if !(self.current)() {
                    return Err(DnsError::Unavailable);
                }
                budget.check().map_err(|_| DnsError::Unavailable)?;
                let mut id = [0; 2];
                self.entropy
                    .fill(&mut id)
                    .map_err(|_| DnsError::Unavailable)?;
                let question = WireQuestion {
                    name,
                    kind: code(kind),
                    id: u16::from_be_bytes(id),
                    edns,
                };
                let response = dns_transport::exchange_with(
                    *server,
                    question,
                    &mut budget,
                    self.packet,
                    self.current,
                    |packet| match decode(packet, question, kind, remaining) {
                        Ok(Message::Truncated) => Ok(Packet::Truncated),
                        Ok(reply) => Ok(Packet::Complete(reply)),
                        Err(WireError::Invalid) => Ok(Packet::Invalid),
                        Err(WireError::Capacity) => Err(Error::Capacity),
                    },
                );
                match response {
                    Ok(Message::Answer(answer)) => return Ok(answer),
                    Ok(Message::ResponseCode(3)) => return Ok(None),
                    Ok(Message::LegacyResolver) if edns => continue,
                    // Retry only remote failures and per-server idle timeouts.
                    // Local resource failures must survive PTR error suppression.
                    Ok(_)
                    | Err(
                        Error::Dns
                        | Error::Deadline
                        | Error::Io {
                            kind:
                                ErrorKind::ConnectionRefused
                                | ErrorKind::ConnectionReset
                                | ErrorKind::ConnectionAborted
                                | ErrorKind::NotConnected
                                | ErrorKind::BrokenPipe
                                | ErrorKind::HostUnreachable
                                | ErrorKind::NetworkUnreachable,
                            ..
                        },
                    ) => {
                        budget.check().map_err(|_| DnsError::Unavailable)?;
                        break;
                    }
                    Err(_) => return Err(DnsError::Unavailable),
                }
            }
        }
        Err(DnsError::Temporary)
    }
}

struct Answer {
    values: Vec<Record>,
    path: Path,
    hops: usize,
}

fn decode(
    packet: &[u8],
    question: WireQuestion<'_>,
    kind: Query,
    remaining: usize,
) -> Result<Message<Option<Answer>>, WireError> {
    dns_reply::parse(packet, question, |rows, _authority| {
        if rows.is_empty() {
            return Ok(None);
        }
        let mut values = Vec::new();
        let (path, hops, _) = dns_reply::walk(packet, question, rows, remaining, |rr| {
            let value = match kind {
                Query::A | Query::Aaaa => Record::Address(dns_reply::address(packet, rr)?),
                Query::Txt => Record::Text(text(dns_reply::value(packet, rr)?)?),
                Query::Mx | Query::Ptr => {
                    let skip = if matches!(kind, Query::Mx) { 2 } else { 0 };
                    let target = dns_reply::target(packet, rr, skip)?;
                    if kind == Query::Ptr && values.len() >= MAX_PTR_NAMES {
                        return Ok(());
                    }
                    Record::Name(presentation(&target)?)
                }
            };
            td_alloc::push(&mut values, value).map_err(|_| WireError::Capacity)
        })?;
        Ok(Some(Answer { values, path, hops }))
    })
}

impl Dns for Resolver<'_> {
    fn lookup(&mut self, name: &str, kind: Query) -> Result<Option<Vec<Record>>, DnsError> {
        let mut name = Name::from_labels(name).map_err(|_| DnsError::Permanent)?;
        let mut seen = [const { None }; MAX_ALIASES];
        let mut hops = 0;
        loop {
            let Some(answer) = self.query(&name, kind, MAX_ALIASES - hops)? else {
                return Ok(None);
            };
            // Each continuation starts at the previous reply's final owner.
            for visited in answer.path.names().take(answer.hops) {
                if seen.iter().flatten().any(|prior| prior == visited) {
                    return Err(DnsError::Temporary);
                }
                *seen.get_mut(hops).ok_or(DnsError::Unavailable)? = Some(*visited);
                hops += 1;
            }
            let last = answer.path.names().last().ok_or(DnsError::Temporary)?;
            if seen.iter().flatten().any(|prior| prior == last) {
                return Err(DnsError::Temporary);
            }
            if !answer.values.is_empty() || answer.hops == 0 {
                return Ok(Some(answer.values));
            }
            name = *last;
        }
    }
}

fn code(kind: Query) -> u16 {
    match kind {
        Query::A => 1,
        Query::Ptr => 12,
        Query::Mx => 15,
        Query::Txt => 16,
        Query::Aaaa => 28,
    }
}

/// TXT segments concatenate within one RR; unrelated RRs stay separate.
fn text(mut bytes: &[u8]) -> Result<Vec<u8>, WireError> {
    if bytes.is_empty() {
        return Err(WireError::Invalid);
    }
    let mut value = td_alloc::with_capacity(bytes.len()).map_err(|_| WireError::Capacity)?;
    while let Some((&len, rest)) = bytes.split_first() {
        let (segment, tail) = rest
            .split_at_checked(usize::from(len))
            .ok_or(WireError::Invalid)?;
        td_alloc::extend_from_slice(&mut value, segment).map_err(|_| WireError::Capacity)?;
        bytes = tail;
    }
    Ok(value)
}

fn presentation(name: &Name) -> Result<String, WireError> {
    if name.wire() == [0] {
        return td_alloc::string(".").map_err(|_| WireError::Capacity);
    }
    let mut bytes = name.wire();
    let mut value = String::new();
    td_alloc::reserve_string(&mut value, bytes.len()).map_err(|_| WireError::Capacity)?;
    while let Some((&len, rest)) = bytes.split_first() {
        if len == 0 {
            break;
        }
        let (label, tail) = rest
            .split_at_checked(usize::from(len))
            .ok_or(WireError::Invalid)?;
        // No presentation escaping in SPF. Preserve label boundaries on lookup.
        if !label
            .iter()
            .all(|b| (0x20..=0x7e).contains(b) && *b != b'.')
        {
            return Err(WireError::Invalid);
        }
        if !value.is_empty() {
            td_alloc::push_str(&mut value, ".").map_err(|_| WireError::Capacity)?;
        }
        let label = std::str::from_utf8(label).map_err(|_| WireError::Invalid)?;
        td_alloc::push_str(&mut value, label).map_err(|_| WireError::Capacity)?;
        bytes = tail;
    }
    Ok(value)
}

#[cfg(test)]
#[path = "../tests/support/spf_dns.rs"]
mod tests;
