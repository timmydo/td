//! Private outgoing copies from one verified, snapshot-bound native message.
use crate::{
    admission::work::{Charge, Meter, Stop},
    ports::{BlobReader, Clock, Deadline, Error, Time},
    root_headers,
    store_fs::PinnedBlob,
    transmitted_headers,
};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Limits {
    pub header_bytes: u64,
    pub message_bytes: u64,
    pub deadline: Deadline,
}
struct LiveClock<'a> {
    source: &'a dyn Clock,
    deadline: Deadline,
    last: AtomicU64,
}
impl Clock for LiveClock<'_> {
    fn sample(&self) -> Result<Time, Error> {
        let time = self.source.sample()?;
        if time.monotonic.0 < self.last.fetch_max(time.monotonic.0, Ordering::Relaxed) {
            return Err(Error::Invalid);
        }
        if self.deadline.expired(time.monotonic) {
            return Err(Error::Deadline);
        }
        Ok(time)
    }
}
fn work(error: Stop) -> Error {
    if error == Stop::Deadline {
        Error::Deadline
    } else {
        Error::Capacity
    }
}
pub(crate) fn header(error: root_headers::Error) -> Error {
    match error {
        root_headers::Error::Policy(error) => error,
        root_headers::Error::Work(error)
        | root_headers::Error::Headers(crate::mime_headers::Error::Work(error)) => work(error),
        root_headers::Error::Capacity
        | root_headers::Error::Headers(crate::mime_headers::Error::HeaderLimit) => Error::Capacity,
        _ => Error::Invalid,
    }
}
/// Retain `source` through this entire operation. `headers` is caller-owned
/// scratch, sized to min(source length, header limit + 2). Sink bytes remain
/// provisional until success; discard them on every error. No sending identity,
/// envelope authorization, staging reservation or queue commit is supplied here.
/// Native integrity verification precedes entry and owns its separate funding.
pub fn write(
    source: &mut PinnedBlob<'_, '_>,
    headers: &mut [u8],
    limits: Limits,
    clock: &dyn Clock,
    meter: &mut Meter,
    sink: impl FnMut(&[u8]) -> Result<(), Error>,
) -> Result<u64, Error> {
    if limits.header_bytes == 0
        || limits.header_bytes > crate::limits::MIB as u64
        || limits.message_bytes == 0
        || meter.deadline() < limits.deadline.into()
    {
        return Err(Error::Invalid);
    }
    let clock = LiveClock {
        source: clock,
        deadline: limits.deadline,
        last: AtomicU64::new(clock.sample()?.monotonic.0),
    };
    source.check_deadline()?;
    let mut cursor = root_headers::Cursor::new(
        source,
        limits.header_bytes,
        headers,
        meter,
        clock.sample()?.monotonic,
    )
    .map_err(header)?;
    loop {
        if cursor.poll(clock.sample()?.monotonic).map_err(header)? == root_headers::Status::Complete
        {
            break;
        }
    }
    let view = cursor.finish(clock.sample()?.monotonic).map_err(header)?;
    write_retained(source, view, limits, &clock, meter, sink)
}
/// Caller retains the same verified root-header view and source pin. This lets
/// envelope validation and copying share one immutable header arena.
pub(crate) fn write_retained(
    source: &mut PinnedBlob<'_, '_>,
    view: root_headers::View<'_>,
    limits: Limits,
    clock: &dyn Clock,
    meter: &mut Meter,
    mut sink: impl FnMut(&[u8]) -> Result<(), Error>,
) -> Result<u64, Error> {
    if view.base != 0
        || view.end.body_start > source.len()
        || view.end.body_start > limits.header_bytes.saturating_add(2)
        || limits.header_bytes == 0
        || limits.header_bytes > crate::limits::MIB as u64
        || limits.message_bytes == 0
        || meter.deadline() < limits.deadline.into()
    {
        return Err(Error::Invalid);
    }
    let clock = LiveClock {
        source: clock,
        deadline: limits.deadline,
        last: AtomicU64::new(clock.sample()?.monotonic.0),
    };
    source.check_deadline()?;
    let mut offset = view.end.body_start;
    let end = usize::try_from(offset).map_err(|_| Error::Invalid)?;
    let section = view.bytes.get(..end).ok_or(Error::Invalid)?;
    let mut written = 0u64;
    let mut emit = |bytes: &[u8]| {
        let next = written
            .checked_add(bytes.len() as u64)
            .filter(|count| *count <= limits.message_bytes)
            .ok_or(Error::Quota)?;
        sink(bytes)?;
        written = next;
        Ok(())
    };
    transmitted_headers::write_checked(
        section,
        || {
            source.check_deadline()?;
            clock.sample()
        },
        limits.deadline,
        meter,
        &mut emit,
    )?;
    source.check_deadline()?;
    let mut buffer = [0; 4096];
    while offset < source.len() {
        let amount = (source.len() - offset).min(buffer.len() as u64) as usize;
        meter
            .charge(
                clock.sample()?.monotonic,
                Charge {
                    io_bytes: amount as u64,
                    output_bytes: amount as u64,
                    records: 1,
                    ..Charge::default()
                },
            )
            .map_err(work)?;
        let count = source.read_at(offset, buffer.get_mut(..amount).ok_or(Error::Invalid)?)?;
        clock.sample()?;
        if count == 0 || count > amount {
            return Err(Error::Corrupt);
        }
        emit(buffer.get(..count).ok_or(Error::Invalid)?)?;
        offset = offset.checked_add(count as u64).ok_or(Error::Capacity)?;
        clock.sample()?;
        source.check_deadline()?;
    }
    meter
        .charge(clock.sample()?.monotonic, Charge::default())
        .map_err(work)?;
    source.check_deadline()?;
    clock.sample()?;
    Ok(written)
}

#[cfg(test)]
#[path = "transmitted_copy_tests.rs"]
mod tests;
