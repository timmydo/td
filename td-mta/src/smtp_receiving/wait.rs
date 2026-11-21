//! Socket waits retain network-owned transports; queued work stays on its worker.
use super::{BoundListener, Connection, Phase, Slot, Storage, Work};
use crate::{
    network_wake::Receiver,
    ports::{Clock, Deadline, Error, Tick},
    readiness_sys::{Connected, Interest, PollSet},
};
use std::{
    sync::{
        atomic::{AtomicU8, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Owner {
    Network,
    Storage,
    Tls,
    Dkim,
}

pub(super) struct Cell<T> {
    pub(super) slot: Mutex<T>,
    owner: AtomicU8,
}
impl<T> Cell<T> {
    pub(super) fn new(slot: T) -> Self {
        Self {
            slot: Mutex::new(slot),
            owner: AtomicU8::new(Owner::Network as u8),
        }
    }
    pub(super) fn assign(&self, owner: Owner) {
        self.owner.store(owner as u8, Ordering::Release);
    }
    pub(super) fn owned(&self, owner: Owner) -> bool {
        self.owner.load(Ordering::Acquire) == owner as u8
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct Blocked {
    pub(super) read: bool,
    pub(super) write: bool,
}

pub(super) fn wait<B: Storage>(
    receiver: &Receiver<'_>,
    cells: &[Cell<Slot<'_, '_, '_, B>>],
    listeners: &[BoundListener<'_>],
    accept_after: Tick,
    stopping: Option<Deadline>,
    scanned: Tick,
    clock: &dyn Clock,
) -> Result<(), Error> {
    let started = Instant::now();
    let time = clock.sample()?;
    // Also recheck the listening certificates at their whole-second precision.
    let mut next =
        Deadline::after(time.monotonic, 1000 - time.utc_ms.rem_euclid(1000) as u64)?.tick();
    if let Some(stop) = stopping.filter(|deadline| !deadline.expired(scanned)) {
        next = next.min(stop.tick());
    }

    let mut guards = [const { None }; crate::limits::MAX_SMTP_SESSIONS];
    for (cell, saved) in cells.iter().zip(&mut guards) {
        // A queued verifier can be cancelled at its deadline before it takes
        // the lock. A running verifier wakes us after publishing its result.
        let slot = if cell.owned(Owner::Network) {
            cell.slot.lock().map_err(|_| Error::WriterStopped)?
        } else if cell.owned(Owner::Dkim) {
            let Ok(guard) = cell.slot.try_lock() else {
                continue;
            };
            guard
        } else {
            continue;
        };
        match slot.phase {
            Phase::Network => {
                let n = slot.connection.as_ref().ok_or(Error::Conflict)?.network()?;
                next = next.min(n.blocked().0.tick());
                *saved = Some(slot);
            }
            Phase::Retry { at, deadline, .. } => next = next.min(at).min(deadline.tick()),
            Phase::AwaitAuthentication { until } => next = next.min(until),
            Phase::Done => return Ok(()),
            _ => (),
        }
    }
    let mut poll = PollSet::new(&receiver.socket);
    for slot in guards.iter().flatten() {
        let socket = match slot.connection.as_ref() {
            Some(Connection::Plain(_, tcp)) => tcp.connected()?,
            Some(Connection::Tls(_, tls)) => tls.socket()?,
            _ => return Err(Error::Conflict),
        };
        poll.add_connected(
            Connected::Tcp(socket),
            Interest {
                read: slot.blocked.read,
                write: slot.blocked.write,
            },
        )?;
    }
    // Even when all slots are occupied this service accepts and writes 421.
    if stopping.is_none() {
        if accept_after > time.monotonic {
            next = next.min(accept_after);
        } else {
            for listener in listeners {
                poll.add_tcp_listener(&listener.socket)?;
            }
        }
    }
    let delay = Duration::from_millis(next.0.saturating_sub(time.monotonic.0));
    poll.wait(started.checked_add(delay).ok_or(Error::Invalid)?)?;
    Ok(())
}

pub(super) fn owner(phase: Phase, verify: bool) -> Owner {
    match phase {
        Phase::Queued {
            work: Work::Tls, ..
        } => Owner::Tls,
        Phase::Retire | Phase::Queued { .. } => Owner::Storage,
        Phase::AwaitAuthentication { .. } if verify => Owner::Dkim,
        _ => Owner::Network,
    }
}

// The common readiness array fits either protocol's configured maximum.
const _: () = assert!(crate::limits::MAX_SMTP_SESSIONS <= crate::limits::MAX_HTTPS_CONNECTIONS);
