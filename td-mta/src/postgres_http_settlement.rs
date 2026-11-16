//! Inline receipt settlement shared by the fixed HTTPS worker's owners.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    ports::{Clock, Error},
    postgres_input::Loaded,
    postgres_probe::{Endpoint, Mode, Request},
};
use std::time::{Duration, Instant};

/// An unreachable pinned endpoint must not hold a write's owner, foreground
/// slot and worker for its whole deadline before the busy refusal.
const INLINE: Duration = Duration::from_secs(3);

/// Prepay `connections` settlement connections, each at the mutation frame
/// ceiling that bounds receipt inspection and Email receipt retirement,
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

/// A runtime connection request for one endpoint, for publication or for
/// settlement. An uncertain operation is settled only where its COMMIT went:
/// POSTGRES.md section 165's absence proof holds on that server alone, so
/// settlement never resolves again or falls back.
pub(crate) fn request(loaded: &Loaded, endpoint: Endpoint) -> Request<'_> {
    let mut connection = loaded.bootstrap.request(endpoint);
    connection.mode = Mode::Runtime;
    connection
}
