//! One observation per receiving slot. Only the SPF worker performs DNS.
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

const LOOKUP_MS: u64 = 2_000;
const FINAL_WAIT_MS: u64 = 200;
pub(crate) const STACK_BYTES: usize = 1024 * 1024;

pub(super) fn wait_until(now: Tick, transport: Deadline, commit_seconds: u64) -> Tick {
    Tick(
        now.0.saturating_add(FINAL_WAIT_MS).min(
            transport
                .tick()
                .0
                .saturating_sub(commit_seconds.saturating_mul(1000)),
        ),
    )
}

struct Query {
    peer: IpAddr,
    helo: String,
    sender: String,
    deadline: Deadline,
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

pub(super) struct Ticket<'a> {
    live: &'a AtomicU64,
    generation: u64,
    deadline: Deadline,
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
    clock: &'a RuntimeClock,
    cells: &'a [Mailbox],
    worker: &'a thread::Thread,
}

impl<'a> Service<'a> {
    pub(super) fn start(
        &self,
        index: usize,
        peer: IpAddr,
        helo: &str,
        sender: &str,
    ) -> Option<Ticket<'a>> {
        let deadline = Deadline::after(self.clock.monotonic().ok()?, LOOKUP_MS).ok()?;
        let item = self.cells.get(index)?;
        let mut cell = item.cell.try_lock().ok()?;
        let generation = cell.generation.checked_add(1)?;
        let query = Query {
            peer,
            helo: td_alloc::string(helo).ok()?,
            sender: td_alloc::string(sender).ok()?,
            deadline,
        };
        *cell = Cell {
            generation,
            queued: Some(query),
            result: None,
        };
        item.live.store(generation, Ordering::Release);
        self.worker.unpark();
        Some(Ticket {
            live: &item.live,
            generation,
            deadline,
        })
    }

    pub(super) fn poll(&self, index: usize, ticket: Option<&Ticket<'_>>) -> Option<Outcome> {
        let Some(ticket) = ticket else {
            return Some(Outcome::Temperror);
        };
        if let Some(cell) = self
            .cells
            .get(index)
            .and_then(|cell| cell.cell.try_lock().ok())
        {
            if cell.generation != ticket.generation {
                return Some(Outcome::Temperror);
            }
            if let Some(result) = cell.result {
                return Some(result);
            }
        }
        self.clock
            .monotonic()
            .map_or(true, |now| ticket.deadline.expired(now))
            .then_some(Outcome::Temperror)
    }
}

fn evaluate(
    query: &Query,
    servers: &[SocketAddr],
    clock: &RuntimeClock,
    packet: &mut [u8],
    current: &dyn Fn() -> bool,
) -> Outcome {
    if clock
        .monotonic()
        .map_or(true, |now| query.deadline.expired(now))
    {
        return Outcome::Temperror;
    }
    let Ok(mut entropy) = td_crypto::SystemEntropy::try_new() else {
        return Outcome::Temperror;
    };
    let Ok(dns) = Resolver::new(servers, clock, query.deadline, &mut entropy, packet) else {
        return Outcome::Temperror;
    };
    let mut dns = dns.while_current(current);
    let outcome = spf::evaluate(&mut dns, query.peer, &query.helo, &query.sender);
    if clock
        .monotonic()
        .is_ok_and(|now| !query.deadline.expired(now))
    {
        outcome
    } else {
        Outcome::Temperror
    }
}

fn consume(
    cells: &[Mailbox],
    stopped: &AtomicBool,
    mut run: impl FnMut(&Query, &dyn Fn() -> bool) -> Outcome,
) {
    loop {
        let mut worked = false;
        for item in cells {
            if stopped.load(Ordering::Acquire) {
                return;
            }
            let job = item
                .cell
                .lock()
                .ok()
                .and_then(|mut cell| cell.queued.take().map(|query| (cell.generation, query)));
            if let Some((generation, query)) = job {
                worked = true;
                let current = || {
                    item.live.load(Ordering::Acquire) == generation
                        && !stopped.load(Ordering::Acquire)
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
            }
        }
        if !worked {
            thread::park();
        }
    }
}

struct Stop<'a>(&'a AtomicBool, &'a thread::Thread);
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
        self.1.unpark();
    }
}

pub(super) fn with_worker<T>(
    slots: usize,
    servers: &[SocketAddr],
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
    let Ok(mut packet) = td_alloc::filled(crate::dns_transport::PACKET_BYTES, 0u8) else {
        return run(None);
    };
    let Ok(name) = td_alloc::string("smtp-spf") else {
        return run(None);
    };
    let stopped = AtomicBool::new(false);
    let clock = RuntimeClock::new();
    thread::scope(|scope| {
        let handle = thread::Builder::new()
            .name(name)
            .stack_size(STACK_BYTES)
            .spawn_scoped(scope, || {
                consume(&cells, &stopped, |query, current| {
                    evaluate(query, servers, &clock, &mut packet, current)
                })
            });
        let Ok(handle) = handle else { return run(None) };
        let stop = Stop(&stopped, handle.thread());
        let result = run(Some(&Service {
            clock: &clock,
            cells: &cells,
            worker: handle.thread(),
        }));
        drop(stop);
        let _ = thread::ScopedJoinHandle::join(handle);
        result
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::{
        net::Ipv4Addr,
        sync::mpsc,
        time::{Duration, Instant},
    };

    #[test]
    fn final_grace_preserves_commit_and_transport_deadlines() {
        let now = Tick(1_000);
        for (remaining_ms, expected) in [(20_000, 1_200), (10_091, 1_091), (10_000, 1_000), (2, 0)]
        {
            let transport = Deadline::after(now, remaining_ms).unwrap();
            assert_eq!(wait_until(now, transport, 10), Tick(expected));
        }
    }

    #[test]
    fn in_flight_result_cannot_cross_mail_transactions() {
        let cells = [Mailbox::default()];
        let clock = RuntimeClock::new();
        let stopped = AtomicBool::new(false);
        let (entered, active) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let cap = Duration::from_secs(3);
        thread::scope(|scope| {
            let h = scope.spawn(|| {
                consume(&cells, &stopped, {
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
            let _stop = Stop(&stopped, h.thread());
            let service = Service {
                clock: &clock,
                cells: &cells,
                worker: h.thread(),
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
    fn worker_skips_discarded_envelopes_before_running_live_successor() {
        let cells: [Mailbox; 2] = std::array::from_fn(|_| Mailbox::default());
        let clock = RuntimeClock::new();
        let stopped = AtomicBool::new(false);
        let current = thread::current();
        let service = Service {
            clock: &clock,
            cells: &cells,
            worker: &current,
        };
        let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let stale = service
            .start(0, peer, "mx.example.test", "old@example.test")
            .unwrap();
        let next = service
            .start(1, peer, "mx.example.test", "live@example.test")
            .unwrap();
        drop(stale);
        let mut calls = 0;
        consume(&cells, &stopped, |query, current| {
            calls += 1;
            assert_eq!(query.sender, "live@example.test");
            assert!(current());
            stopped.store(true, Ordering::Release);
            Outcome::Pass
        });
        assert_eq!(calls, 1);
        drop(next);
    }
}
