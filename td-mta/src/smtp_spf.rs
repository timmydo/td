//! One observation per receiving slot. Only the SPF workers perform DNS.
use crate::{
    clock::RuntimeClock,
    ports::{Clock, Deadline, Tick},
    spf::{self, Outcome},
    spf_dns::Resolver,
};
use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
    thread,
};

/// Each observation's own budget, counted from the moment a worker starts it.
const LOOKUP_MS: u64 = 2_000;
/// Fairness rests on oldest-first order and the final-dot wait bound; more
/// workers shorten that bound.
pub(crate) const WORKERS: usize = 3;
pub(crate) const STACK_BYTES: usize = 1024 * 1024;

/// Workers take the oldest queued envelope. Ahead of one are at most one
/// envelope per other slot and one finishing or abandoned job per worker,
/// each holding its worker for at most `LOOKUP_MS`; then its own turn.
fn final_wait_ms(sessions: usize, workers: usize) -> u64 {
    let workers = workers.max(1);
    let ahead = sessions.saturating_sub(1).saturating_add(workers);
    let rounds = u64::try_from(ahead.div_ceil(workers)).unwrap_or(u64::MAX);
    rounds.saturating_add(1).saturating_mul(LOOKUP_MS)
}

pub(super) fn wait_until(
    now: Tick,
    transport: Deadline,
    commit_seconds: u64,
    wait_ms: u64,
) -> Tick {
    Tick(
        now.0.saturating_add(wait_ms).min(
            transport
                .tick()
                .0
                .saturating_sub(commit_seconds.saturating_mul(1000)),
        ),
    )
}

struct Query {
    order: u64,
    peer: IpAddr,
    helo: String,
    sender: String,
}

#[derive(Default)]
struct Cell {
    generation: u64,
    queued: Option<Query>,
    result: Option<Outcome>,
}

#[derive(Default)]
struct Mailbox {
    cell: Mutex<Cell>,
    live: AtomicU64,
}

struct Shared {
    cells: Vec<Mailbox>,
    order: AtomicU64,
    stopped: AtomicBool,
}

impl Shared {
    fn new(cells: Vec<Mailbox>) -> Self {
        Self {
            cells,
            order: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
        }
    }

    /// Remove the oldest queued envelope. Losing it to another worker means
    /// that worker progressed, so scan again rather than sleeping.
    fn take_oldest(&self) -> Option<(&Mailbox, u64, Query)> {
        loop {
            let mut oldest: Option<(&Mailbox, u64)> = None;
            for item in &self.cells {
                let Ok(cell) = item.cell.lock() else {
                    continue;
                };
                if let Some(order) = cell.queued.as_ref().map(|query| query.order) {
                    if oldest.is_none_or(|(_, prior)| order < prior) {
                        oldest = Some((item, order));
                    }
                }
            }
            let (item, order) = oldest?;
            let Ok(mut cell) = item.cell.lock() else {
                continue;
            };
            if cell.queued.as_ref().map(|query| query.order) == Some(order) {
                let generation = cell.generation;
                if let Some(query) = cell.queued.take() {
                    return Some((item, generation, query));
                }
            }
        }
    }
}

pub(super) struct Ticket<'a> {
    live: &'a AtomicU64,
    generation: u64,
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        // Discard without taking a worker lock or invalidating a later MAIL.
        let _ = self
            .live
            .compare_exchange(self.generation, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}

pub(super) struct Service<'a> {
    shared: &'a Shared,
    workers: &'a [Option<&'a thread::Thread>],
}

impl<'a> Service<'a> {
    pub(super) fn start(
        &self,
        index: usize,
        peer: IpAddr,
        helo: &str,
        sender: &str,
    ) -> Option<Ticket<'a>> {
        let item = self.shared.cells.get(index)?;
        // Workers hold a cell lock only for constant-time bookkeeping.
        let mut cell = item.cell.lock().ok()?;
        let generation = cell.generation.checked_add(1)?;
        let query = Query {
            order: self.shared.order.fetch_add(1, Ordering::Relaxed),
            peer,
            helo: td_alloc::string(helo).ok()?,
            sender: td_alloc::string(sender).ok()?,
        };
        *cell = Cell {
            generation,
            queued: Some(query),
            result: None,
        };
        item.live.store(generation, Ordering::Release);
        drop(cell);
        for worker in self.workers.iter().flatten() {
            worker.unpark();
        }
        Some(Ticket {
            live: &item.live,
            generation,
        })
    }

    /// The final-dot wait bound for the workers that actually started.
    pub(super) fn final_wait_ms(&self, sessions: usize) -> u64 {
        final_wait_ms(sessions, self.workers.iter().flatten().count())
    }

    /// None while the observation is queued or running. The receiver bounds
    /// that wait and records `temperror` if the wait ends first.
    pub(super) fn poll(&self, index: usize, ticket: Option<&Ticket<'_>>) -> Option<Outcome> {
        let Some(ticket) = ticket else {
            return Some(Outcome::Temperror);
        };
        let cell = self.shared.cells.get(index)?.cell.lock().ok()?;
        if cell.generation != ticket.generation {
            return Some(Outcome::Temperror);
        }
        cell.result
    }
}

fn evaluate(
    query: &Query,
    servers: &[SocketAddr],
    clock: &RuntimeClock,
    packet: &mut [u8],
    current: &dyn Fn() -> bool,
) -> Outcome {
    // Time queued behind other sessions is not charged to this one.
    let Some(deadline) = clock
        .monotonic()
        .ok()
        .and_then(|now| Deadline::after(now, LOOKUP_MS).ok())
    else {
        return Outcome::Temperror;
    };
    let Ok(mut entropy) = td_crypto::SystemEntropy::try_new() else {
        return Outcome::Temperror;
    };
    let Ok(dns) = Resolver::new(servers, clock, deadline, &mut entropy, packet) else {
        return Outcome::Temperror;
    };
    let mut dns = dns.while_current(current);
    let outcome = spf::evaluate(&mut dns, query.peer, &query.helo, &query.sender);
    if clock.monotonic().is_ok_and(|now| !deadline.expired(now)) {
        outcome
    } else {
        Outcome::Temperror
    }
}

fn consume(
    shared: &Shared,
    completed: impl Fn(),
    mut run: impl FnMut(&Query, &dyn Fn() -> bool) -> Outcome,
) {
    while !shared.stopped.load(Ordering::Acquire) {
        let Some((item, generation, query)) = shared.take_oldest() else {
            thread::park();
            continue;
        };
        let current = || {
            item.live.load(Ordering::Acquire) == generation
                && !shared.stopped.load(Ordering::Acquire)
        };
        if !current() {
            continue;
        }
        let result = run(&query, &current);
        if let Ok(mut cell) = item.cell.lock() {
            if current() {
                cell.result = Some(result);
            }
        }
        completed();
    }
}

struct Stop<'a>(&'a AtomicBool, &'a [Option<&'a thread::Thread>]);
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
        for worker in self.1.iter().flatten() {
            worker.unpark();
        }
    }
}

pub(super) fn with_worker<T>(
    slots: usize,
    servers: &[SocketAddr],
    wake: &crate::network_wake::Notifications,
    run: impl FnOnce(Option<&Service<'_>>) -> T,
) -> T {
    if servers.is_empty() || slots == 0 {
        return run(None);
    }
    let Ok(mut cells) = td_alloc::with_capacity(slots) else {
        return run(None);
    };
    for _ in 0..slots {
        if td_alloc::push(&mut cells, Mailbox::default()).is_err() {
            return run(None);
        }
    }
    let mut packets: [Vec<u8>; WORKERS] = Default::default();
    for packet in &mut packets {
        let Ok(bytes) = td_alloc::filled(crate::dns_transport::PACKET_BYTES, 0u8) else {
            return run(None);
        };
        *packet = bytes;
    }
    let shared = Shared::new(cells);
    let clock = RuntimeClock::new();
    thread::scope(|scope| {
        let mut handles = [const { None }; WORKERS];
        for (packet, handle) in packets.iter_mut().zip(&mut handles) {
            let Ok(name) = td_alloc::string("smtp-spf") else {
                break;
            };
            let (shared, clock) = (&shared, &clock);
            let spawned = thread::Builder::new()
                .name(name)
                .stack_size(STACK_BYTES)
                .spawn_scoped(scope, move || {
                    consume(
                        shared,
                        || wake.notify(),
                        |query, current| evaluate(query, servers, clock, packet, current),
                    )
                });
            let Ok(spawned) = spawned else {
                break;
            };
            *handle = Some(spawned);
        }
        let threads = handles
            .each_ref()
            .map(|handle| handle.as_ref().map(thread::ScopedJoinHandle::thread));
        let stop = Stop(&shared.stopped, &threads);
        let result = if threads.iter().any(Option::is_some) {
            run(Some(&Service {
                shared: &shared,
                workers: &threads,
            }))
        } else {
            run(None)
        };
        drop(stop);
        for handle in handles.into_iter().flatten() {
            let _ = thread::ScopedJoinHandle::join(handle);
        }
        result
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::{
        net::{Ipv4Addr, UdpSocket},
        sync::mpsc,
        time::{Duration, Instant},
    };

    fn shared(slots: usize) -> Shared {
        Shared::new((0..slots).map(|_| Mailbox::default()).collect())
    }

    #[test]
    fn final_grace_preserves_commit_and_transport_deadlines() {
        let now = Tick(1_000);
        for (remaining_ms, expected) in [(20_000, 1_200), (10_091, 1_091), (10_000, 1_000), (2, 0)]
        {
            let transport = Deadline::after(now, remaining_ms).unwrap();
            assert_eq!(wait_until(now, transport, 10, 200), Tick(expected));
        }
    }

    #[test]
    fn final_wait_covers_a_full_queue_and_the_observation_itself() {
        // Thirty-two default sessions: thirty-one queued, three finishing,
        // then this one.
        assert_eq!(
            final_wait_ms(crate::limits::Limits::default().smtp_sessions, WORKERS),
            13 * LOOKUP_MS
        );
        assert_eq!(final_wait_ms(8, WORKERS), 5 * LOOKUP_MS);
        assert_eq!(final_wait_ms(1, WORKERS), 2 * LOOKUP_MS);
        assert_eq!(
            final_wait_ms(crate::limits::MAX_SMTP_SESSIONS, WORKERS),
            23 * LOOKUP_MS
        );
        // Fewer started workers lengthen the bound rather than void it.
        assert_eq!(final_wait_ms(8, 1), 9 * LOOKUP_MS);
        assert_eq!(final_wait_ms(8, 0), 9 * LOOKUP_MS);
    }

    #[test]
    fn in_flight_result_cannot_cross_mail_transactions() {
        let shared = shared(1);
        let (entered, active) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let cap = Duration::from_secs(3);
        thread::scope(|scope| {
            let h = scope.spawn(|| {
                consume(&shared, || (), {
                    move |q, current| {
                        if q.sender == "first@example.test" {
                            entered.send(()).unwrap();
                            if blocked.recv_timeout(cap).is_ok() {
                                assert!(!current());
                                return Outcome::Fail;
                            }
                        }
                        Outcome::Pass
                    }
                })
            });
            let workers = [Some(h.thread())];
            let _stop = Stop(&shared.stopped, &workers);
            let service = Service {
                shared: &shared,
                workers: &workers,
            };
            let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
            let first = service
                .start(0, peer, "mx.example.test", "first@example.test")
                .unwrap();
            active.recv_timeout(cap).unwrap();
            assert_eq!(service.poll(0, Some(&first)), None);
            let second = service
                .start(0, peer, "mx.example.test", "next@example.test")
                .unwrap();
            assert_eq!(service.poll(0, Some(&first)), Some(Outcome::Temperror));
            assert_eq!(service.poll(0, Some(&second)), None);
            drop(first); // A late discard must leave the second generation live.
            release.send(()).unwrap();
            let stop = Instant::now() + cap;
            loop {
                if let Some(result) = service.poll(0, Some(&second)) {
                    assert_eq!(result, Outcome::Pass);
                    break;
                }
                assert!(Instant::now() < stop);
                thread::yield_now();
            }
        });
    }

    #[test]
    fn worker_skips_discarded_envelopes_and_takes_the_oldest_live_one() {
        let shared = shared(3);
        let workers = [None];
        let service = Service {
            shared: &shared,
            workers: &workers,
        };
        let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let stale = service
            .start(0, peer, "mx.example.test", "old@example.test")
            .unwrap();
        // Slot 2 queues before slot 1, so it runs first despite its index.
        let older = service
            .start(2, peer, "mx.example.test", "older@example.test")
            .unwrap();
        let newer = service
            .start(1, peer, "mx.example.test", "newer@example.test")
            .unwrap();
        drop(stale);
        let mut calls = Vec::new();
        consume(
            &shared,
            || (),
            |query, current| {
                assert!(current());
                calls.push(query.sender.clone());
                if calls.len() == 2 {
                    shared.stopped.store(true, Ordering::Release);
                }
                Outcome::Pass
            },
        );
        assert_eq!(calls, ["older@example.test", "newer@example.test"]);
        drop((older, newer));
    }

    /// Names under `slow.example.test` answer after 900 ms, inside the
    /// resolver's idle cap; others answer at once with `v=spf1 +all`.
    fn serve(socket: &UdpSocket, done: &AtomicBool) {
        const SLOW: &[u8] = b"\x04slow\x07example\x04test\x00";
        while !done.load(Ordering::Acquire) {
            let mut packet = [0; 512];
            let Ok((n, peer)) = socket.recv_from(&mut packet) else {
                continue;
            };
            let (_, length) = crate::dns_wire::Name::read(packet.get(..n).unwrap(), 12).unwrap();
            let question = 12 + length + 4;
            let name = packet.get(12..12 + length).unwrap().to_vec();
            let mut answer = packet.get(..question).unwrap().to_vec();
            *answer.get_mut(2).unwrap() |= 0x80;
            if packet.get(question - 3) == Some(&16) {
                let text: &[u8] = if name.ends_with(SLOW) {
                    b"v=spf1 include:a.slow.example.test include:b.slow.example.test -all"
                } else {
                    b"v=spf1 +all"
                };
                *answer.get_mut(7).unwrap() = 1;
                answer.extend_from_slice(&[0xc0, 12, 0, 16, 0, 1, 0, 0, 0, 60, 0]);
                answer.push(u8::try_from(text.len() + 1).unwrap());
                answer.push(u8::try_from(text.len()).unwrap());
                answer.extend_from_slice(text);
            }
            answer.extend_from_slice(packet.get(question..n).unwrap());
            if name.ends_with(SLOW) {
                let delayed = socket.try_clone().unwrap();
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(900));
                    let _ = delayed.send_to(&answer, peer);
                });
            } else {
                socket.send_to(&answer, peer).unwrap();
            }
        }
    }

    #[test]
    fn slow_sender_dns_does_not_spend_another_senders_budget() {
        let done = AtomicBool::new(false);
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let address = socket.local_addr().unwrap();
        let wake = crate::network_wake::Notifications::default();
        let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
        thread::scope(|scope| {
            scope.spawn(|| serve(&socket, &done));
            with_worker(WORKERS + 2, &[address], &wake, |service| {
                let service = service.unwrap();
                // More slow senders than workers: the ordinary sender queues
                // behind one that holds a worker past its own two seconds.
                let slow: Vec<_> = (0..=WORKERS)
                    .map(|slot| {
                        service
                            .start(slot, peer, "mx.slow.example.test", "a@slow.example.test")
                            .unwrap()
                    })
                    .collect();
                let started = Instant::now();
                let ordinary = service
                    .start(WORKERS + 1, peer, "mx.example.test", "b@fast.example.test")
                    .unwrap();
                let until = started + Duration::from_secs(15);
                let result = loop {
                    if let Some(result) = service.poll(WORKERS + 1, Some(&ordinary)) {
                        break result;
                    }
                    assert!(Instant::now() < until);
                    thread::sleep(Duration::from_millis(5));
                };
                assert_eq!(result, Outcome::Pass);
                // It started only after a slow observation released a worker.
                // Worker deadlines are whole milliseconds; allow that rounding.
                assert!(started.elapsed() >= Duration::from_millis(LOOKUP_MS - 50));
                for (slot, ticket) in slow.iter().enumerate() {
                    let result = loop {
                        if let Some(result) = service.poll(slot, Some(ticket)) {
                            break result;
                        }
                        assert!(Instant::now() < until);
                        thread::sleep(Duration::from_millis(5));
                    };
                    assert_eq!(result, Outcome::Temperror);
                }
            });
            done.store(true, Ordering::Release);
        });
    }
}
