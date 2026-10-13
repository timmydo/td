//! Validate direct MIME locators and count/stream with one original work meter.
use crate::{
    admission::work::{Meter, Stop},
    blob_structure,
    https_exchange::Sender,
    limits::Limits,
    mime_input,
    mime_response::locators,
    mime_structure,
    nfc::{Credit, HeaderBudget},
    ports::{BlobReader, Clock, Error},
    store_fs::PinnedBlob,
    wire::PartLocator,
};
pub(super) struct Prepared {
    locator: PartLocator,
    pub length: u64,
}
fn buffer<T: Default + Clone>(size: usize) -> Result<Vec<T>, Error> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).map_err(|_| Error::Capacity)?;
    bytes.resize(size, T::default());
    Ok(bytes)
}
fn stopped(stop: Stop) -> Error {
    match stop {
        Stop::Deadline => Error::Deadline,
        _ => Error::Capacity,
    }
}
fn decode(error: mime_input::Error) -> Error {
    match error {
        mime_input::Error::Work(stop) => stopped(stop),
        mime_input::Error::Policy(error) => error,
        _ => Error::NotFound,
    }
}
fn structure(error: crate::blob_entity::Error) -> Error {
    match error {
        crate::blob_entity::Error::Work(stop) => stopped(stop),
        crate::blob_entity::Error::Policy(error) => error,
        _ => Error::NotFound,
    }
}
pub(super) fn prepare(
    body: &mut PinnedBlob<'_, '_>,
    locator: PartLocator,
    limits: &Limits,
    work: &mut Meter,
    clock: &dyn Clock,
    sender: &mut Sender<'_>,
) -> Result<Prepared, Error> {
    locator
        .checked_end(body.len())
        .map_err(|_| Error::NotFound)?;
    let arena_bytes = limits
        .header_bytes
        .checked_add(limits.mime_parts)
        .and_then(|bytes| bytes.checked_add(2))
        .ok_or(Error::Capacity)?;
    let mut arena = buffer(arena_bytes)?;
    let mut parts = buffer(limits.mime_parts)?;
    let mut headers = buffer(limits.mime_parts)?;
    let mut input = [0; mime_input::INPUT_BYTES];
    let mut budget = HeaderBudget::new();
    let length = body.len();
    let mut cursor = blob_structure::Cursor::new(
        body,
        blob_structure::Settings {
            extent: 0..length,
            limits: mime_structure::Limits {
                header_bytes: limits.header_bytes,
                mime_depth: limits.mime_depth,
                mime_parts: limits.mime_parts,
            },
        },
        blob_structure::Buffers {
            header_bytes: &mut arena,
            body: &mut input,
            parts: &mut parts,
            headers: &mut headers,
        },
        work,
        &mut budget,
        clock.sample()?.monotonic,
    )
    .map_err(structure)?;
    while cursor.poll(clock.sample()?.monotonic).map_err(structure)?
        != mime_structure::Status::Complete
    {
        sender.check()?;
    }
    let retained = cursor
        .finish(clock.sample()?.monotonic)
        .map_err(structure)?;
    let mut credit = Credit::new();
    let mut found = false;
    for part in retained.parts {
        sender.check()?;
        let candidate = locators::candidate(
            *part,
            part.ordinal,
            locators::Parent {
                id: locator.parent,
                base: 0,
                length,
            },
            retained.work,
            retained.budget,
            &mut credit,
            clock.sample()?.monotonic,
        )
        .map_err(|error| match error {
            crate::mime_response::bound::Error::Admission(crate::nfc::Error::Work(stop)) => {
                stopped(stop)
            }
            crate::mime_response::bound::Error::Admission(_) => Error::Capacity,
            _ => Error::NotFound,
        })?;
        if candidate.locator() == Some(locator) {
            found = true;
            break;
        }
    }
    retained.source.check_deadline()?;
    if !found {
        return Err(Error::NotFound);
    }
    // Count decoded octets before publishing Content-Length; replay keeps work.
    let before = retained.work.remaining();
    let mut reader = mime_input::Reader::with_source(
        retained.source,
        locator.offset,
        locator.length,
        locator.encoding,
        &mut input,
    )
    .map_err(decode)?;
    let mut output = [0; 4096];
    loop {
        sender.check()?;
        let progress = reader
            .poll(clock, retained.work, &mut output)
            .map_err(decode)?;
        if progress.status == crate::mime_base64::Status::Complete {
            break;
        }
    }
    let length = reader.position();
    reader.release().map_err(decode)?.source.check_deadline()?;
    let remaining = retained.work.remaining();
    for (initial, left) in [
        (before.io_bytes, remaining.io_bytes),
        (before.records, remaining.records),
        (before.output_bytes, remaining.output_bytes),
        (before.unlinks, remaining.unlinks),
    ] {
        if left < initial.checked_sub(left).ok_or(Error::Corrupt)? {
            return Err(Error::Capacity);
        }
    }
    sender.check()?;
    Ok(Prepared { locator, length })
}
pub(super) fn send(
    body: &mut PinnedBlob<'_, '_>,
    part: Prepared,
    work: &mut Meter,
    clock: &dyn Clock,
    sender: &mut Sender<'_>,
    output: &mut [u8],
) -> Result<(), Error> {
    let mut input = [0; mime_input::INPUT_BYTES];
    let mut reader = mime_input::Reader::with_source(
        body,
        part.locator.offset,
        part.locator.length,
        part.locator.encoding,
        &mut input,
    )
    .map_err(decode)?;
    loop {
        sender.check()?;
        let progress = reader.poll(clock, work, output).map_err(decode)?;
        if reader.position() > part.length {
            return Err(Error::Corrupt);
        }
        if progress.written != 0 {
            sender.write(output.get(..progress.written).ok_or(Error::Corrupt)?)?;
            output
                .get_mut(..progress.written)
                .ok_or(Error::Corrupt)?
                .fill(0);
        }
        if progress.status == crate::mime_base64::Status::Complete {
            break;
        }
    }
    if reader.position() != part.length {
        return Err(Error::Corrupt);
    }
    reader.release().map_err(decode)?.source.check_deadline()?;
    sender.check()
}
