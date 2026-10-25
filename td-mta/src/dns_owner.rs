//! Configured-name Resolver port for one exclusively owned fixed worker.
use crate::{
    clock::RuntimeClock,
    config::{certificate, outbound},
    dns_lookup,
    dns_reply::{MAX_ADDRESSES, MAX_ALIASES},
    dns_transport::{Budget, PACKET_BYTES},
    ports::{Clock, Deadline, Entropy, Error, Resolution, Resolver, Tick},
};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroU64,
    sync::Arc,
};
use td_mime::work::{Charge, Meter};

const SLOTS: usize = 2;
const WORK: Charge = Charge {
    records: 4_000_000,
    io_bytes: 64 * 1024 * 1024,
    output_bytes: 1024 * 1024,
    unlinks: 0,
};
#[derive(Clone, Copy)]
struct Entry {
    generation: NonZeroU64,
    index: usize,
    values: [IpAddr; MAX_ADDRESSES],
    resolution: Resolution,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Binding {
    generation: NonZeroU64,
    names: [Option<crate::dns_wire::Name>; SLOTS],
    servers: [SocketAddr; outbound::MAX_RESOLVERS],
    server_count: usize,
}

/// Two bounded endpoint entries shared across attempts on one runtime clock.
#[derive(Default)]
pub struct Cache {
    clock: Option<Arc<RuntimeClock>>,
    binding: Option<Binding>,
    entries: [Option<Entry>; SLOTS],
}
impl Cache {
    fn bind(&mut self, clock: &Arc<RuntimeClock>, binding: Binding) -> Result<(), Error> {
        if self
            .clock
            .as_ref()
            .is_some_and(|held| !Arc::ptr_eq(held, clock))
        {
            return Err(Error::Invalid);
        }
        if self.binding.as_ref() != Some(&binding) {
            self.entries.fill(None);
            self.binding = Some(binding);
        }
        if self.clock.is_none() {
            self.clock = Some(clock.clone());
        }
        Ok(())
    }
}

/// Build on the owning worker from the active configuration. Entropy must be
/// initialized there; packet storage is the single reserved resolver cell.
/// Configuration borrows and generation remain fixed for this owner's life.
pub struct Configured<'a, E> {
    clock: &'a RuntimeClock,
    entropy: E,
    generation: NonZeroU64,
    names: [Option<&'a str>; SLOTS],
    servers: [SocketAddr; outbound::MAX_RESOLVERS],
    server_count: usize,
    cache: &'a mut Cache,
    packet: &'a mut [u8],
}
impl<'a, E: Entropy> Configured<'a, E> {
    pub fn new(
        outbound: outbound::View<'_, 'a>,
        certificates: certificate::View<'_, 'a>,
        generation: NonZeroU64,
        clock: &'a Arc<RuntimeClock>,
        entropy: E,
        packet: &'a mut [u8],
        cache: &'a mut Cache,
    ) -> Result<Self, Error> {
        if cache
            .clock
            .as_ref()
            .is_some_and(|held| !Arc::ptr_eq(held, clock))
        {
            return Err(Error::Invalid);
        }
        let packet = packet.get_mut(..PACKET_BYTES).ok_or(Error::Capacity)?;
        let relay = outbound.relay().map_err(|_| Error::Invalid)?;
        let acme = certificates.acme().map_err(|_| Error::Invalid)?;
        let mut servers = [SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)); outbound::MAX_RESOLVERS];
        let server_count = outbound.resolver_count();
        if server_count == 0 || server_count > servers.len() {
            return Err(Error::Invalid);
        }
        for index in 0..server_count {
            *servers.get_mut(index).ok_or(Error::Invalid)? = outbound
                .resolver(index)
                .map_err(|_| Error::Invalid)?
                .ok_or(Error::Invalid)?
                .endpoint;
        }
        let names = [Some(relay.host), acme.map(|acme| acme.directory.host())];
        let binding = Binding {
            generation,
            names: [
                names
                    .first()
                    .copied()
                    .flatten()
                    .map(crate::dns_wire::Name::from_host)
                    .transpose()
                    .map_err(|_| Error::Invalid)?,
                names
                    .get(1)
                    .copied()
                    .flatten()
                    .map(crate::dns_wire::Name::from_host)
                    .transpose()
                    .map_err(|_| Error::Invalid)?,
            ],
            servers,
            server_count,
        };
        cache.bind(clock, binding)?;
        Ok(Self {
            clock,
            entropy,
            generation,
            names,
            servers,
            server_count,
            cache,
            packet,
        })
    }
    fn now(&self, deadline: Deadline) -> Result<Tick, Error> {
        let now = self.clock.sample()?.monotonic;
        if deadline.expired(now) {
            Err(Error::Deadline)
        } else {
            Ok(now)
        }
    }
    fn index(&self, host: &str) -> Result<usize, Error> {
        self.names
            .iter()
            .position(|name| name.is_some_and(|name| name.eq_ignore_ascii_case(host)))
            .ok_or(Error::Forbidden)
    }
    fn cached(&self, index: usize, now: Tick) -> Option<Entry> {
        self.cache
            .entries
            .get(index)
            .copied()
            .flatten()
            .filter(|entry| {
                entry.generation == self.generation
                    && entry.index == index
                    && entry.resolution.valid_until > now
            })
    }
}
impl<E: Entropy> Resolver for Configured<'_, E> {
    fn resolve(
        &mut self,
        host: &str,
        deadline: Deadline,
        output: &mut [IpAddr],
    ) -> Result<Resolution, Error> {
        self.now(deadline)?;
        let index = self.index(host)?;
        if output.len() < MAX_ADDRESSES {
            return Err(Error::Capacity);
        }
        let now = self.now(deadline)?;
        let cached = self.cached(index, now);
        let from_cache = cached.is_some();
        let entry = match cached {
            Some(entry) => entry,
            None => {
                // Retire expired data before any fallible lookup. There is no
                // stale-on-error path, negative cache or background refresh.
                *self.cache.entries.get_mut(index).ok_or(Error::Invalid)? = None;
                let configured = self
                    .names
                    .get(index)
                    .copied()
                    .flatten()
                    .ok_or(Error::Invalid)?;
                let mut values = [IpAddr::V4(Ipv4Addr::UNSPECIFIED); MAX_ADDRESSES];
                let mut meter = Meter::new(deadline, WORK);
                let mut budget = Budget {
                    clock: self.clock,
                    deadline,
                    meter: &mut meter,
                    remaining_aliases: MAX_ALIASES,
                };
                let resolution = dns_lookup::lookup(
                    configured,
                    self.servers
                        .get(..self.server_count)
                        .ok_or(Error::Invalid)?,
                    &mut self.entropy,
                    &mut budget,
                    self.packet,
                    &mut values,
                )?;
                let entry = Entry {
                    generation: self.generation,
                    index,
                    values,
                    resolution,
                };
                let now = self.now(deadline)?;
                if resolution.valid_until > now {
                    *self.cache.entries.get_mut(index).ok_or(Error::Invalid)? = Some(entry);
                }
                entry
            }
        };
        let now = self.now(deadline)?;
        if from_cache && entry.resolution.valid_until <= now {
            *self.cache.entries.get_mut(index).ok_or(Error::Invalid)? = None;
            return Err(Error::Dns);
        }
        output
            .get_mut(..entry.resolution.count)
            .ok_or(Error::Capacity)?
            .copy_from_slice(
                entry
                    .values
                    .get(..entry.resolution.count)
                    .ok_or(Error::Invalid)?,
            );
        Ok(entry.resolution)
    }
}
const _: () = assert!(std::mem::size_of::<Entry>() <= 384);
const _: () = assert!(std::mem::size_of::<Cache>() <= 2048);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::config::{syntax::Location, text};
    use std::{cell::Cell, net::UdpSocket, num::NonZeroU32, rc::Rc, thread, time::Duration};
    #[derive(Clone)]
    struct Random {
        count: Rc<Cell<u16>>,
        fail: Rc<Cell<bool>>,
    }
    impl Entropy for Random {
        fn fill(&mut self, output: &mut [u8]) -> Result<(), crate::ports::CryptoError> {
            if self.fail.get() {
                return Err(crate::ports::CryptoError::Entropy);
            }
            let n = self.count.get() + 1;
            self.count.set(n);
            output.copy_from_slice(&n.to_be_bytes());
            Ok(())
        }
    }
    fn random() -> Random {
        Random {
            count: Rc::new(Cell::new(0)),
            fail: Rc::new(Cell::new(false)),
        }
    }
    fn fixture<R>(
        endpoint: SocketAddr,
        run: impl FnOnce(outbound::View<'_, '_>, certificate::View<'_, '_>) -> R,
    ) -> R {
        let _guard = text::TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut bytes = vec![0; 8192];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let at = Location {
            line: NonZeroU32::new(1).unwrap(),
            column: 1,
        };
        let mut out = outbound::Builder::new(&arena);
        out.resolver(&mut arena, "fixture", &endpoint.to_string(), at)
            .unwrap();
        out.relay(
            &mut arena,
            outbound::RelayInput {
                route: None,
                host: "Relay.Test",
                port: 465,
                transport: outbound::Transport::ImplicitTls,
                username: "operator",
                password_file: "/fixture/password",
                ca_file: None,
            },
            at,
        )
        .unwrap();
        let out = out.finish().unwrap();
        let mut slots = [certificate::Slot::EMPTY; 1];
        let mut cert = certificate::Builder::new(&arena, &mut slots).unwrap();
        cert.acme(
            &mut arena,
            certificate::AcmeInput {
                directory: "https://Ca.Test/directory",
                contact: "operator@example.test",
                terms_accepted: true,
                ca_file: None,
            },
            at,
        )
        .unwrap();
        cert.profile(&mut arena, "managed", certificate::ProfileInput::Acme, at)
            .unwrap();
        let cert = cert.finish().unwrap();
        run(
            out.view_live(&arena).unwrap(),
            cert.view_live(&arena).unwrap(),
        )
    }
    fn answer(query: &[u8], ttl: u32) -> Vec<u8> {
        let mut bytes = query.to_vec();
        bytes[2] |= 0x80;
        bytes[7] = 1;
        let kind = query[query.len() - 3];
        bytes.extend_from_slice(&[0xc0, 12, 0, kind, 0, 1]);
        bytes.extend_from_slice(&ttl.to_be_bytes());
        if kind == 1 {
            bytes.extend_from_slice(&[0, 4, 192, 0, 2, 1]);
        } else {
            bytes.extend_from_slice(&[0, 16]);
            bytes.extend_from_slice(&[0x20, 1, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        }
        bytes
    }
    #[test]
    fn rebuilt_resolvers_reuse_positive_cache_but_not_zero_ttl_or_missing_family() {
        for (ttl, both) in [(0, true), (60, true), (60, false)] {
            let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let endpoint = socket.local_addr().unwrap();
            thread::scope(|scope| {
                scope.spawn(|| {
                    for _ in 0..if ttl == 0 || !both { 4 } else { 2 } {
                        let mut bytes = [0; 512];
                        let (size, peer) = socket.recv_from(&mut bytes).unwrap();
                        let mut response = answer(&bytes[..size], ttl);
                        if !both && bytes[size - 3] == 28 {
                            response.truncate(size);
                            response[7] = 0;
                        }
                        socket.send_to(&response, peer).unwrap();
                    }
                });
                fixture(endpoint, |out, cert| {
                    let clock = Arc::new(RuntimeClock::new());
                    let mut cache = Cache::default();
                    let entropy = random();
                    let count = entropy.count.clone();
                    let mut packet = vec![0; PACKET_BYTES];
                    let deadline =
                        Deadline::after(clock.sample().unwrap().monotonic, 2000).unwrap();
                    let sentinel = IpAddr::from([203, 0, 113, 9]);
                    let mut output = [sentinel; MAX_ADDRESSES];
                    let mut expiry = None;
                    for host in ["relay.test", "RELAY.TEST"] {
                        let mut owner = Configured::new(
                            out,
                            cert,
                            NonZeroU64::new(1).unwrap(),
                            &clock,
                            entropy.clone(),
                            &mut packet,
                            &mut cache,
                        )
                        .unwrap();

                        let result = owner.resolve(host, deadline, &mut output).unwrap();
                        assert_eq!(result.count, if both { 2 } else { 1 });
                        assert!(output[0].is_ipv4());
                        if both {
                            assert!(output[1].is_ipv6());
                        } else {
                            assert_eq!(output[1], sentinel);
                        }
                        if ttl != 0 && both {
                            if let Some(expiry) = expiry {
                                assert_eq!(result.valid_until, expiry);
                            }
                            expiry = Some(result.valid_until);
                        }
                        assert!(output[2..].iter().all(|a| *a == sentinel));
                    }
                    assert_eq!(count.get(), if ttl == 0 || !both { 4 } else { 2 });
                    let mut owner = Configured::new(
                        out,
                        cert,
                        NonZeroU64::new(1).unwrap(),
                        &clock,
                        entropy,
                        &mut packet,
                        &mut cache,
                    )
                    .unwrap();
                    assert_eq!(owner.cache.entries[0].is_some(), ttl != 0 && both);
                    let before = output;
                    assert_eq!(
                        owner.resolve("incoming.example.test", deadline, &mut output),
                        Err(Error::Forbidden)
                    );
                    assert_eq!(
                        owner.resolve("relay.test.", deadline, &mut output),
                        Err(Error::Forbidden)
                    );
                    assert_eq!(output, before);
                    assert_eq!(owner.index("ca.test"), Ok(1));
                });
            });
        }
    }
    #[test]
    fn cache_binding_retires_changed_configuration_and_rejects_clock_domains() {
        let clock = Arc::new(RuntimeClock::new());
        let generation = NonZeroU64::new(1).unwrap();
        let binding = Binding {
            generation,
            names: [
                Some(crate::dns_wire::Name::from_host("Relay.Test").unwrap()),
                Some(crate::dns_wire::Name::from_host("Ca.Test").unwrap()),
            ],
            servers: [SocketAddr::from((Ipv4Addr::LOCALHOST, 53)); outbound::MAX_RESOLVERS],
            server_count: 1,
        };
        let entry = Entry {
            generation,
            index: 0,
            values: [IpAddr::from([192, 0, 2, 1]); MAX_ADDRESSES],
            resolution: Resolution {
                count: 1,
                valid_until: Tick(1000),
            },
        };
        let mut cache = Cache::default();
        cache.bind(&clock, binding.clone()).unwrap();
        cache.entries[0] = Some(entry);
        let mut case_only = binding.clone();
        case_only.names[0] = Some(crate::dns_wire::Name::from_host("relay.test").unwrap());
        cache.bind(&clock.clone(), case_only).unwrap();
        assert!(cache.entries[0].is_some());
        assert_eq!(
            cache.bind(&Arc::new(RuntimeClock::new()), binding.clone()),
            Err(Error::Invalid)
        );
        assert!(cache.entries[0].is_some());
        let mut variants = Vec::new();
        let mut changed = binding.clone();
        changed.generation = NonZeroU64::new(2).unwrap();
        variants.push(changed);
        for index in 0..SLOTS {
            let mut changed = binding.clone();
            changed.names[index] = Some(crate::dns_wire::Name::from_host("other.test").unwrap());
            variants.push(changed);
        }
        let mut changed = binding.clone();
        changed.names[1] = None;
        variants.push(changed);
        let mut changed = binding.clone();
        changed.servers[0].set_port(5353);
        variants.push(changed);
        let mut changed = binding.clone();
        changed.servers[0].set_ip(IpAddr::from([127, 0, 0, 2]));
        variants.push(changed);
        let mut changed = binding.clone();
        changed.server_count = 2;
        variants.push(changed);
        for changed in variants {
            cache.bind(&clock, binding.clone()).unwrap();
            cache.entries[0] = Some(entry);
            cache.bind(&clock, changed).unwrap();
            assert!(cache.entries.iter().all(Option::is_none));
        }
    }

    #[test]
    fn cache_keys_expiry_and_refusals_preserve_output() {
        fixture(SocketAddr::from((Ipv4Addr::LOCALHOST, 53)), |out, cert| {
            let clock = Arc::new(RuntimeClock::new());
            let mut cache = Cache::default();
            let entropy = random();
            let count = entropy.count.clone();
            let fail = entropy.fail.clone();
            let mut packet = vec![0; PACKET_BYTES];
            let generation = NonZeroU64::new(4).unwrap();
            let mut owner = Configured::new(
                out,
                cert,
                generation,
                &clock,
                entropy,
                &mut packet,
                &mut cache,
            )
            .unwrap();
            let deadline = Deadline::after(clock.sample().unwrap().monotonic, 2000).unwrap();
            let entry = Entry {
                generation,
                index: 0,
                values: [IpAddr::from([192, 0, 2, 1]); MAX_ADDRESSES],
                resolution: Resolution {
                    count: 1,
                    valid_until: Tick(1000),
                },
            };
            owner.cache.entries[0] = Some(entry);
            assert!(owner.cached(0, Tick(999)).is_some());
            assert!(owner.cached(0, Tick(1000)).is_none());
            assert!(owner.cached(1, Tick(0)).is_none());
            owner.cache.entries[0] = Some(Entry {
                generation: NonZeroU64::new(5).unwrap(),
                ..entry
            });
            assert!(owner.cached(0, Tick(0)).is_none());
            owner.cache.entries[0] = Some(Entry { index: 1, ..entry });
            assert!(owner.cached(0, Tick(0)).is_none());
            owner.cache.entries[0] = Some(Entry {
                resolution: Resolution {
                    valid_until: Tick(0),
                    ..entry.resolution
                },
                ..entry
            });
            fail.set(true);
            let before = [IpAddr::from([203, 0, 113, 9]); MAX_ADDRESSES];
            let mut output = before;
            assert_eq!(
                owner.resolve("relay.test", deadline, &mut output),
                Err(Error::Entropy)
            );
            assert!(owner.cache.entries[0].is_none());
            assert_eq!(
                owner.resolve(
                    "relay.test",
                    Deadline::after(Tick(0), 0).unwrap(),
                    &mut output
                ),
                Err(Error::Deadline)
            );
            assert_eq!(
                owner.resolve("relay.test", deadline, &mut output[..1]),
                Err(Error::Capacity)
            );
            assert_eq!(output, before);
            assert_eq!(count.get(), 0);
        });
    }
}
