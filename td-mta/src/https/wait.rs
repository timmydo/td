//! The network owner borrows sockets only while their client slot is locked.
use super::{BoundListener, Client, Connection, Deadline, Error, ListenerSocket, Phase, Tick};
use crate::network_wake::Receiver;
use crate::readiness_sys::{Connected, Interest, PollSet};
use std::{
    sync::{
        atomic::{AtomicU8, Ordering},
        Mutex, MutexGuard, TryLockError,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Owner {
    Network,
    Tls,
    Auth,
    Event,
    Body,
}
impl Owner {
    pub(super) fn phase(phase: Phase) -> Self {
        match phase {
            Phase::Tls => Self::Tls,
            Phase::Authenticate => Self::Auth,
            Phase::EventRead => Self::Event,
            Phase::Api | Phase::Download | Phase::Upload => Self::Body,
            _ => Self::Network,
        }
    }
}

pub(super) struct Slot {
    pub(super) client: Mutex<Option<Client>>,
    owner: AtomicU8,
}
impl Slot {
    pub(super) fn empty() -> Self {
        Self {
            client: Mutex::new(None),
            owner: AtomicU8::new(Owner::Network as u8),
        }
    }
    pub(super) fn owned(&self, role: Owner) -> bool {
        self.owner.load(Ordering::Acquire) == role as u8
    }
    pub(super) fn assign(&self, role: Owner) {
        self.owner.store(role as u8, Ordering::Release);
    }
    pub(super) fn try_client(&self) -> Result<Option<MutexGuard<'_, Option<Client>>>, Error> {
        match self.client.try_lock() {
            Ok(g) => Ok(Some(g)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(_) => Err(Error::WriterStopped),
        }
    }
    pub(super) fn leave_worker(
        &self,
        guard: MutexGuard<'_, Option<Client>>,
        control: &super::Control,
    ) {
        self.assign(Owner::Network);
        drop(guard);
        control.https_wake.notify();
    }
    pub(super) fn network_client(&self) -> Result<Option<MutexGuard<'_, Option<Client>>>, Error> {
        if self.owned(Owner::Network) {
            Ok(Some(self.client.lock().map_err(|_| Error::WriterStopped)?))
        } else {
            Ok(None)
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Wait {
    Again,
    Socket { read: bool, write: bool },
}
impl Wait {
    pub(super) const READ: Self = Self::Socket {
        read: true,
        write: false,
    };
    pub(super) const WRITE: Self = Self::Socket {
        read: false,
        write: true,
    };
}

pub(super) fn wait(
    receiver: &Receiver<'_>,
    slots: &[Slot],
    listeners: &[BoundListener],
    accept_after: Tick,
    clock: &dyn crate::ports::Clock,
) -> Result<(), Error> {
    use crate::limits::MAX_HTTPS_SLOTS;
    // Network-owned guards pin their transports while ppoll borrows the fds.
    // Worker-owned slots stay unlocked for all database, TLS and body work.
    let started = Instant::now();
    let time = clock.sample()?;
    let mut guards = [const { None }; MAX_HTTPS_SLOTS];
    let mut free = false;
    // Wall-clock changes can invalidate a listening certificate. Its boundary
    // has whole-second precision. Traffic and stop always wake earlier.
    let horizon = if listeners.iter().any(|l| l.policy.is_some()) {
        1000 - time.utc_ms.rem_euclid(1000) as u64
    } else {
        3_600_000
    };
    let mut next = Deadline::after(time.monotonic, horizon)?.tick();
    for (slot, saved) in slots.iter().zip(&mut guards) {
        if let Some(guard) = slot.network_client()? {
            let Some(client) = guard.as_ref() else {
                free = true;
                continue;
            };
            if matches!(client.wait, Wait::Again) {
                return Ok(());
            }
            next = next.min(client.deadline().tick());
            if let Some(event) = &client.events {
                if let Some(Ok(sample)) = &event.sample {
                    next = next.min(sample.expires.tick());
                }
                if !event.headers && !event.pending {
                    if let Some(ping) = event.stream.as_ref().and_then(|stream| stream.next_ping())
                    {
                        next = next.min(ping);
                    }
                }
            }
            *saved = Some(guard);
        } else if slot.owned(Owner::Event) {
            if let Some(guard) = slot.try_client()? {
                if let Some(client) = guard.as_ref() {
                    next = next.min(client.deadline().tick());
                }
            }
        }
    }
    let mut poll = PollSet::new(&receiver.socket);
    for client in guards.iter().flatten().filter_map(|guard| guard.as_ref()) {
        if let Wait::Socket { read, write } = client.wait {
            let socket = match (&client.connection, &client.pending) {
                (Some(Connection::Local(c)), None) => Connected::Unix(c.connected()?),
                (Some(Connection::Tls(c)), None) => Connected::Tcp(c.socket()?),
                (None, Some(socket)) if client.phase == Phase::Hello => Connected::Tcp(socket),
                _ => return Err(Error::Invalid),
            };
            poll.add_connected(socket, Interest { read, write })?;
        }
    }
    if free {
        if accept_after > time.monotonic {
            next = next.min(accept_after);
        } else {
            for listener in listeners {
                match &listener.socket {
                    ListenerSocket::Tcp(l) => poll.add_tcp_listener(l)?,
                    ListenerSocket::Unix(l) => poll.add_unix_listener(l.socket())?,
                }
            }
        }
    }
    let delay = Duration::from_millis(next.0.saturating_sub(time.monotonic.0));
    poll.wait(started.checked_add(delay).ok_or(Error::Invalid)?)?;
    Ok(())
}
