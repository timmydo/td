//! Inline receipt settlement shared by the HTTPS and SMTP publication owners.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    ids::AccountId,
    ports::{Clock, Error},
    postgres_input::Loaded,
    postgres_probe::{Mode, Request},
};
use std::time::{Duration, Instant};

/// An unreachable database must not hold a write's owner, foreground slot
/// and worker for its whole deadline before the busy refusal.
const INLINE: Duration = Duration::from_secs(3);

/// Historical evidence can be inspected on a later request, but only during
/// a bounded monotonic interval and only on behalf of its own account.
const HTTP_INSPECTION_WINDOW: Duration =
    Duration::from_millis(crate::postgres_receipt_expiry::RETENTION_MS as u64);

pub(crate) struct Retained<T> {
    pub(crate) change: T,
    account: AccountId,
    since: Instant,
}

impl<T> Retained<T> {
    pub(crate) fn new(change: T, account: AccountId) -> Self {
        Self {
            change,
            account,
            since: Instant::now(),
        }
    }

    /// Discarding local inspection data neither determines the old outcome
    /// nor authorizes replay. The new request must run its own transaction.
    pub(crate) fn inspect_before_write(
        slot: &mut Option<Self>,
        account: AccountId,
        now: Instant,
    ) -> bool {
        if slot.as_ref().is_some_and(|held| {
            held.account != account
                || now.saturating_duration_since(held.since) >= HTTP_INSPECTION_WINDOW
        }) {
            *slot = None;
        }
        slot.is_some()
    }
}

/// Prepay `connections` settlement connections, each at the mutation frame
/// ceiling that bounds receipt inspection,
/// before a write settles its owner's retained operation.
pub(crate) fn charge(
    meter: &mut Meter,
    clock: &RuntimeClock,
    connections: u64,
) -> Result<(), Error> {
    meter
        .charge(
            clock.sample()?.monotonic,
            Charge {
                records: 1088 * connections,
                ..Charge::default()
            },
        )
        .map_err(|error| {
            if error == Stop::Deadline {
                Error::Deadline
            } else {
                Error::Capacity
            }
        })
}

/// The deadline for settlement run inline by a write: the write's own,
/// capped at three seconds from now.
pub(crate) fn inline_deadline(write: Instant) -> Result<Instant, Error> {
    Ok(write.min(Instant::now().checked_add(INLINE).ok_or(Error::Invalid)?))
}

/// A runtime connection request over the bootstrap's socket, for publication
/// or for settlement. Retained operations live no longer than the bootstrap
/// they were published through, so settlement inspects the server their
/// COMMIT went to.
pub(crate) fn request(loaded: &Loaded) -> Request<'_> {
    let mut connection = loaded.bootstrap.request();
    connection.mode = Mode::Runtime;
    connection
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_requests_can_release_unresolved_history() {
        let account = AccountId::from_bytes([1; 16]);
        let other = AccountId::from_bytes([2; 16]);
        let new = || Retained::new((), account);
        let held = new();
        let since = held.since;
        let mut slot = Some(held);
        assert!(Retained::inspect_before_write(&mut slot, account, since));
        assert!(!Retained::inspect_before_write(&mut slot, other, since));
        assert!(slot.is_none());

        let held = new();
        let end = held.since + HTTP_INSPECTION_WINDOW;
        let mut slot = Some(held);
        assert!(Retained::inspect_before_write(
            &mut slot,
            account,
            end - Duration::from_millis(1)
        ));
        assert!(!Retained::inspect_before_write(&mut slot, account, end));
        assert!(slot.is_none());
    }
}
