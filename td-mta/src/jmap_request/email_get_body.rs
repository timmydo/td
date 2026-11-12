//! Bounded per-message composition over one verified account snapshot.
use super::{storage, Error, Selection, Worker};
#[path = "email_search_body.rs"]
mod search;
use crate::{
    admission::work::Meter,
    blob_structure,
    body_value_options::Options,
    limits::Limits,
    mime_body_lists::Node,
    mime_part_headers::{self, label_json},
    mime_response::{locators::Candidate, part_json::End, root_header_json},
    mime_structure::{self, Part},
    nfc::{HeaderBudget, Scratch},
    performance::{Outcome, Phase, Timer},
    ports::{BlobReader, Clock, Tick},
    store_fs::PinnedBlob,
};
pub use search::collect_verified_search;
pub(in crate::jmap_request::dispatch) use search::search_verified;

fn buffer<T: Default + Copy>(
    memory: &td_alloc::Budget,
    count: usize,
) -> Result<td_alloc::Buffer<T>, Error> {
    td_alloc::Buffer::filled(memory, count, T::default()).map_err(|_| Error::ResponseCapacity)
}
pub(super) fn now(clock: &dyn Clock) -> Result<Tick, Error> {
    clock.monotonic().map_err(storage)
}
trait MimeError {
    fn dispatch(self) -> Error;
}
fn mime<E: MimeError>(error: E) -> Error {
    error.dispatch()
}
macro_rules! mime_error {
    ($ty:path, $($variant:ident => $map:path),* $(,)?) => {
        impl MimeError for $ty {
            fn dispatch(self) -> Error {
                match self {
                    $(Self::$variant(error) => $map(error),)*
                    _ => Error::MessageInterpretation,
                }
            }
        }
    };
}
impl MimeError for crate::admission::work::Stop {
    fn dispatch(self) -> Error {
        match self {
            Self::Deadline => Error::ExecutionDeadline,
            _ => Error::StorageCapacity,
        }
    }
}
mime_error!(crate::header_property::Error, Work => mime);
mime_error!(crate::root_headers::Error, Policy => storage, Work => mime);
mime_error!(crate::mime_response::root_header_json::Error,
    Read => mime, Policy => storage, Work => mime);
mime_error!(crate::blob_entity::Error, Headers => mime, Policy => storage, Work => mime);
mime_error!(crate::blob_part::Error, Policy => storage);
mime_error!(crate::mime_response::pinned::Error, Parent => storage);
mime_error!(crate::blob_body_values::Error,
    Policy => storage, Metadata => mime, Value => mime);
mime_error!(crate::blob_selected_preview::Error,
    Policy => storage, Metadata => mime, Preview => mime, Work => mime);
mime_error!(crate::blob_preview::Error, Policy => storage, Json => mime);
mime_error!(crate::blob_body_value::Error, Policy => storage, Json => mime);
mime_error!(crate::preview_json::Error, Policy => storage, Text => mime, Work => mime);
mime_error!(crate::body_value::json::Error,
    Policy => storage, Text => mime, Html => mime, Work => mime);
mime_error!(crate::body_value::html::Error, Policy => storage, Work => mime);
mime_error!(crate::mime_text::Error, Policy => storage, Input => mime, Work => mime);
mime_error!(crate::mime_input::Error, Policy => storage, Work => mime);

struct Metadata<'a> {
    heads: &'a mut [u8],
    charset: &'a mut [u8],
    filename: &'a mut [u8],
    id: &'a mut [u8],
    language: &'a mut [u8],
    location: &'a mut [u8],
}
fn metadata_bytes(limit: usize) -> Result<usize, Error> {
    limit
        .checked_mul(6)
        .and_then(|n| n.checked_add(14))
        .ok_or(Error::ResponseCapacity)
}
impl<'a> Metadata<'a> {
    fn new(bytes: &'a mut [u8], limit: usize) -> Result<Self, Error> {
        let bytes = bytes
            .get_mut(..metadata_bytes(limit)?)
            .ok_or(Error::ResponseCapacity)?;
        let (heads, bytes) = bytes
            .split_at_mut_checked(limit.checked_add(14).ok_or(Error::ResponseCapacity)?)
            .ok_or(Error::ResponseCapacity)?;
        let (charset, bytes) = bytes
            .split_at_mut_checked(limit)
            .ok_or(Error::ResponseCapacity)?;
        let (filename, bytes) = bytes
            .split_at_mut_checked(limit)
            .ok_or(Error::ResponseCapacity)?;
        let (id, bytes) = bytes
            .split_at_mut_checked(limit)
            .ok_or(Error::ResponseCapacity)?;
        let (language, location) = bytes
            .split_at_mut_checked(limit)
            .ok_or(Error::ResponseCapacity)?;
        Ok(Self {
            heads,
            charset,
            filename,
            id,
            language,
            location,
        })
    }
    fn backing(&mut self) -> label_json::Backing<'_> {
        label_json::Backing {
            headers: mime_part_headers::Backing {
                heads: self.heads,
                charset: self.charset,
                filename: self.filename,
            },
            labels: crate::mime_label_fields::json::Backing {
                content_id: self.id,
                content_language: self.language,
            },
            content_location: self.location,
        }
    }
}
fn reborrow<'a, 'v, 'c, 'b: 'a, 'o: 'a>(
    r: &'a mut blob_structure::Retained<'_, 'v, 'c, 'b, 'o, '_>,
) -> blob_structure::Retained<'a, 'v, 'c, 'a, 'a, 'a> {
    blob_structure::Retained {
        source: r.source,
        work: r.work,
        budget: r.budget,
        parts: r.parts,
        headers: r.headers,
        remaining: r.remaining,
        header_bytes: r.header_bytes,
    }
}

/// Emit the existing Email MIME/header members from verified snapshot custody.
/// Each member starts with a comma; the caller owns object framing and stored
/// fields. Output is provisional until this and the enclosing request succeed.
/// The output owner must enforce original response admission and execution
/// deadlines; this function also retains the pin and supplied work allowance.
pub fn write_verified(
    output: &mut (impl Output + ?Sized),
    pin: &mut PinnedBlob<'_, '_>,
    limits: &Limits,
    selection: (&Selection, Options),
    clock: &dyn Clock,
    work: &mut Meter,
) -> Result<(), Error> {
    write_with_evidence(output, pin, limits, selection, clock, work, None)
}

pub(crate) fn write_with_evidence(
    output: &mut (impl Output + ?Sized),
    pin: &mut PinnedBlob<'_, '_>,
    limits: &Limits,
    selection: (&Selection, Options),
    clock: &dyn Clock,
    work: &mut Meter,
    evidence: Option<&crate::postgres_mime_evidence::Evidence>,
) -> Result<(), Error> {
    let result = write_verified_inner(output, pin, limits, selection, clock, work, evidence);
    output.check()?;
    if let Some(stop) = work.stopped() {
        return Err(mime(stop));
    }
    result
}
struct HeaderBacking<'a> {
    extent: std::ops::Range<u64>,
    arena: &'a mut [u8],
    budget: &'a mut HeaderBudget,
    scratch: &'a mut Scratch,
}
fn write_headers(
    worker: &mut impl Output,
    pin: &mut PinnedBlob<'_, '_>,
    settings: (&Limits, &[super::Name]),
    clock: &dyn Clock,
    work: &mut Meter,
    backing: HeaderBacking<'_>,
) -> Result<(), Error> {
    let (limits, names) = settings;
    let mut chunk = [0; 4096];
    if !names.is_empty() {
        // Selection admits at most 64 names. Passive descriptors borrow those
        // names; a fixed table needs neither a second owner nor heap growth.
        let mut properties = [None; 64];
        for (index, name) in names.iter().enumerate() {
            let name = std::str::from_utf8(name.bytes()).map_err(|_| Error::State)?;
            let mut parser =
                crate::header_property::Cursor::new(name, crate::header_property::Context::Email);
            loop {
                if let crate::header_property::Status::Complete(property) =
                    parser.poll(now(clock)?, work).map_err(mime)?
                {
                    *properties.get_mut(index).ok_or(Error::ResponseCapacity)? =
                        Some(property.ok_or(Error::State)?);
                    break;
                }
                worker.check()?;
            }
        }
        let mut reader = crate::root_headers::Cursor::new_extent(
            pin,
            backing.extent,
            limits.header_bytes as u64,
            backing.arena,
            work,
            now(clock)?,
        )
        .map_err(mime)?;
        while reader.poll(now(clock)?).map_err(mime)? != crate::root_headers::Status::Complete {
            worker.check()?;
        }
        let mut reader = reader
            .into_json(backing.budget, backing.scratch, now(clock)?)
            .map_err(mime)?;
        for property in properties.into_iter().flatten() {
            worker.check()?;
            let mut key = [0; 1538];
            let mut key = crate::bounded::TextBuffer::new(&mut key);
            crate::jmap_session::quoted(&mut key, property.requested()).map_err(storage)?;
            worker.append(b",")?;
            worker.append(key.as_bytes().map_err(|_| Error::State)?)?;
            worker.append(b":")?;
            let mut cursor = reader
                .project(root_header_json::Selection::Value(property), now(clock)?)
                .map_err(mime)?;
            loop {
                let progress = cursor.poll(now(clock)?, &mut chunk).map_err(mime)?;
                worker.append(chunk.get(..progress.written).ok_or(Error::State)?)?;
                if progress.status == root_header_json::Status::Complete {
                    break;
                }
            }
            cursor.finish(now(clock)?).map_err(mime)?;
        }
        reader.finish(now(clock)?).map_err(mime)?;
    }
    Ok(())
}
fn write_verified_inner(
    output: &mut (impl Output + ?Sized),
    pin: &mut PinnedBlob<'_, '_>,
    limits: &Limits,
    selection: (&Selection, Options),
    clock: &dyn Clock,
    work: &mut Meter,
    evidence: Option<&crate::postgres_mime_evidence::Evidence>,
) -> Result<(), Error> {
    output.check()?;
    pin.check_deadline().map_err(storage)?;
    if pin.len() > crate::postgres_body::MAX_BODY_BYTES {
        return Err(Error::StorageCapacity);
    }
    let (selected, options) = selection;
    let memory = selected.headers.budget();
    let header_timing = Timer::start(Phase::JmapHeaders);
    let mut buffered = Buffered {
        inner: output,
        bytes: [0; 4096],
        used: 0,
    };
    let worker = &mut buffered;
    let mut chunk = [0; 4096];
    let mut budget = HeaderBudget::new();
    let mut scratch = Scratch::new();
    let arena_bytes = limits
        .header_bytes
        .checked_add(limits.mime_parts)
        .and_then(|bytes| bytes.checked_add(2))
        .ok_or(Error::ResponseCapacity)?;
    let mut arena = buffer(memory, arena_bytes)?;
    let extent = 0..pin.len();
    write_headers(
        worker,
        pin,
        (limits, &selected.headers),
        clock,
        work,
        HeaderBacking {
            extent,
            arena: &mut arena,
            budget: &mut budget,
            scratch: &mut scratch,
        },
    )?;
    header_timing.finish(Outcome::Success);
    if !selected.body() {
        pin.check_deadline().map_err(storage)?;
        worker.flush()?;
        return pin.check_deadline().map_err(storage);
    }
    let structure_timing = Timer::start(Phase::JmapStructure);
    let parse_limits = mime_structure::Limits {
        header_bytes: limits.header_bytes,
        mime_depth: limits.mime_depth,
        mime_parts: limits.mime_parts,
    };
    let mut parts: td_alloc::Buffer<Part> = buffer(memory, limits.mime_parts)?;
    let mut headers = buffer(memory, limits.mime_parts)?;
    let length = pin.len();
    let buffers = blob_structure::Buffers {
        header_bytes: &mut arena,
        body: &mut chunk,
        parts: &mut parts,
        headers: &mut headers,
    };
    let mut original = if let Some(evidence) = evidence {
        let retained =
            blob_structure::from_evidence(pin, evidence, buffers, work, &mut budget, clock)
                .map_err(mime)?;
        crate::performance::record(
            Phase::StructureReuse,
            std::time::Duration::ZERO,
            Outcome::Success,
        );
        retained
    } else {
        let mut cursor = blob_structure::Cursor::new(
            pin,
            blob_structure::Settings {
                extent: 0..length,
                limits: parse_limits,
            },
            buffers,
            work,
            &mut budget,
            now(clock)?,
        )
        .map_err(mime)?;
        while cursor.poll_batch(clock).map_err(mime)? != mime_structure::Status::Complete {
            worker.check()?;
        }
        cursor.finish(now(clock)?).map_err(mime)?
    };
    let count = original.parts.len();
    let metadata_limit = limits.header_bytes.min(16 * 1024);
    let mut metadata_storage = buffer(memory, metadata_bytes(metadata_limit)?)?;
    let mut metadata = Metadata::new(&mut metadata_storage, metadata_limit)?;
    let mut nodes: td_alloc::Buffer<Node> = buffer(memory, count)?;
    let mut facts =
        td_alloc::BudgetVec::with_capacity(memory, count).map_err(|_| Error::ResponseCapacity)?;
    for index in 0..count {
        worker.check()?;
        let ordinal = u16::try_from(index + 1).map_err(|_| Error::State)?;
        let mut cursor = crate::blob_part::Cursor::new(
            reborrow(&mut original),
            ordinal,
            metadata.backing(),
            &mut scratch,
            now(clock)?,
        )
        .map_err(mime)?;
        while cursor.poll(now(clock)?).map_err(mime)? != mime_part_headers::Status::Complete {
            worker.check()?;
        }
        let part = cursor.finish(now(clock)?).map_err(mime)?;
        *nodes.get_mut(index).ok_or(Error::State)? = part.metadata.node;
        facts
            .try_push(crate::blob_value_parts::Fact {
                end: End {
                    part: part.metadata.part,
                    node: part.metadata.node,
                },
                kind: crate::blob_value_parts::Kind::from_headers(&part.metadata.metadata.headers),
            })
            .map_err(|_| Error::ResponseCapacity)?;
    }
    let mut candidates: td_alloc::Buffer<Candidate> = buffer(memory, count)?;
    let mut locators =
        crate::blob_part_locators::Cursor::new(original, &mut candidates, now(clock)?)
            .map_err(mime)?;
    while locators.poll(now(clock)?).map_err(mime)? != crate::blob_part_locators::Status::Complete {
        worker.check()?;
    }
    let original = locators.finish(now(clock)?).map_err(mime)?.structure;
    let (mut text, mut html, mut attachments, mut membership) = (
        buffer(memory, count)?,
        buffer(memory, count)?,
        buffer(memory, count)?,
        buffer(memory, count)?,
    );
    let mut cursor = crate::blob_body_lists::Cursor::new(
        original,
        &nodes,
        parse_limits,
        crate::mime_body_lists::Backing {
            text: &mut text,
            html: &mut html,
            attachments: &mut attachments,
            membership: &mut membership,
        },
        now(clock)?,
    )
    .map_err(mime)?;
    while cursor.poll(now(clock)?).map_err(mime)? != crate::mime_body_lists::Status::Complete {
        worker.check()?;
    }
    let mut original = cursor.finish(now(clock)?).map_err(mime)?;
    let mut part_output = PartOutput {
        candidates: &candidates,
        fragment: buffer(memory, 192 * 1024)?,
    };
    for (wanted, name, ordinals) in [
        (selected.text, b"textBody".as_slice(), original.lists.text),
        (selected.html, b"htmlBody".as_slice(), original.lists.html),
        (
            selected.attachments,
            b"attachments".as_slice(),
            original.lists.attachments,
        ),
    ] {
        if !wanted {
            continue;
        }
        worker.append(b",\"")?;
        worker.append(name)?;
        worker.append(b"\":[")?;
        for (position, ordinal) in ordinals.iter().enumerate() {
            if position != 0 {
                worker.append(b",")?;
            }
            write_part(
                worker,
                &mut original.structure,
                *ordinal,
                &mut metadata,
                &mut scratch,
                &mut part_output,
                clock,
            )?;
        }
        worker.append(b"]")?;
    }
    structure_timing.finish(Outcome::Success);
    let mut decode_input = buffer(memory, crate::mime_input::INPUT_BYTES)?;
    let mut checkpoints = crate::mime_input::Checkpoints::default();
    if selected.preview {
        let timing = Timer::start(Phase::JmapPreview);
        let original = crate::blob_body_lists::Retained {
            structure: reborrow(&mut original.structure),
            nodes: original.nodes,
            lists: original.lists,
        };
        let mut choice =
            crate::blob_preview_selection::Cursor::new(original, now(clock)?).map_err(mime)?;
        while choice.poll(now(clock)?).map_err(mime)?
            != crate::blob_preview_selection::Status::Complete
        {
            worker.check()?;
        }
        let choice = choice.finish(now(clock)?).map_err(mime)?;
        worker.append(b",\"preview\":")?;
        let mut preview = crate::blob_selected_preview::Cursor::new(
            choice,
            crate::blob_selected_preview::Buffers {
                metadata: metadata.backing(),
                scratch: &mut scratch,
                input: &mut decode_input,
                checkpoints: &mut checkpoints,
            },
            clock,
            now(clock)?,
        )
        .map_err(mime)?;
        loop {
            let p = preview.poll_batch(now(clock)?, &mut chunk).map_err(mime)?;
            worker.append(chunk.get(..p.written).ok_or(Error::State)?)?;
            if p.status == crate::blob_selected_preview::Status::Complete {
                break;
            }
        }
        preview.finish(now(clock)?).map_err(mime)?;
        timing.finish(Outcome::Success);
    }
    if selected.values {
        let timing = Timer::start(Phase::JmapBodyValues);
        let original = crate::blob_body_lists::Retained {
            structure: reborrow(&mut original.structure),
            nodes: original.nodes,
            lists: original.lists,
        };
        let mut ordinals = buffer(memory, count)?;
        let mut marks = buffer(memory, count)?;
        let mut selector = crate::blob_value_parts::Cursor::new(
            original,
            &facts,
            &mut ordinals,
            &mut marks,
            options,
            now(clock)?,
        )
        .map_err(mime)?;
        while selector.poll(now(clock)?).map_err(mime)? != crate::blob_value_parts::Status::Complete
        {
            worker.check()?;
        }
        let retained = selector.finish(now(clock)?).map_err(mime)?;
        let mut values =
            crate::blob_body_values::Cursor::new(retained, now(clock)?).map_err(mime)?;
        worker.append(b",")?;
        loop {
            let p = values.poll(now(clock)?, &mut chunk).map_err(mime)?;
            worker.append(chunk.get(..p.written).ok_or(Error::State)?)?;
            match p.status {
                crate::blob_body_values::Status::Complete => break,
                crate::blob_body_values::Status::NeedValue(_) => {
                    let mut value = values
                        .begin_value(
                            crate::blob_body_values::Buffers {
                                metadata: metadata.backing(),
                                scratch: &mut scratch,
                                input: &mut decode_input,
                                checkpoints: &mut checkpoints,
                            },
                            clock,
                            now(clock)?,
                        )
                        .map_err(mime)?;
                    loop {
                        let p = value.poll_batch(now(clock)?, &mut chunk).map_err(mime)?;
                        worker.append(chunk.get(..p.written).ok_or(Error::State)?)?;
                        if p.status == crate::blob_body_value::Status::Complete {
                            break;
                        }
                    }
                    value.finish(now(clock)?).map_err(mime)?;
                }
                _ => (),
            }
        }
        values.finish(now(clock)?).map_err(mime)?;
        timing.finish(Outcome::Success);
    }
    original
        .structure
        .source
        .check_deadline()
        .map_err(storage)?;
    worker.flush()?;
    original.structure.source.check_deadline().map_err(storage)
}
struct PartOutput<'a> {
    candidates: &'a [Candidate],
    fragment: td_alloc::Buffer<u8>,
}
fn write_part(
    worker: &mut impl Output,
    original: &mut blob_structure::Retained<'_, '_, '_, '_, '_, '_>,
    ordinal: u16,
    metadata: &mut Metadata<'_>,
    scratch: &mut Scratch,
    output: &mut PartOutput<'_>,
    clock: &dyn Clock,
) -> Result<(), Error> {
    let mut cursor = crate::blob_part::Cursor::new(
        reborrow(original),
        ordinal,
        metadata.backing(),
        scratch,
        now(clock)?,
    )
    .map_err(mime)?;
    while cursor.poll(now(clock)?).map_err(mime)? != mime_part_headers::Status::Complete {
        worker.check()?;
    }
    let mut cursor =
        crate::blob_part_json::Cursor::new(cursor.finish(now(clock)?).map_err(mime)?, now(clock)?)
            .map_err(mime)?;
    let fragment = &mut output.fragment;
    let mut filled = 0;
    loop {
        let p = cursor
            .poll(now(clock)?, fragment.get_mut(filled..).ok_or(Error::State)?)
            .map_err(mime)?;
        filled += p.written;
        worker.check()?;
        if p.status == crate::blob_part_json::Status::Complete {
            break;
        }
        if filled == fragment.len() {
            return Err(Error::MessageInterpretation);
        }
    }
    let part = cursor.finish(now(clock)?).map_err(mime)?;
    let candidate = output
        .candidates
        .get(usize::from(ordinal).checked_sub(1).ok_or(Error::State)?)
        .ok_or(Error::State)?;
    let properties = crate::mime_response::selected_metadata::Properties {
        part_id: true,
        blob_id: true,
        media_type: true,
        name: true,
        size: true,
        ..crate::mime_response::selected_metadata::Properties::NONE
    };
    let mut cursor = crate::blob_selected_metadata::Cursor::new(
        part,
        fragment.get(..filled).ok_or(Error::State)?,
        candidate,
        properties,
        now(clock)?,
    )
    .map_err(mime)?;
    worker.append(b"{")?;
    let mut chunk = [0; 512];
    loop {
        let p = cursor.poll(now(clock)?, &mut chunk).map_err(mime)?;
        worker.append(chunk.get(..p.written).ok_or(Error::State)?)?;
        if p.status == crate::blob_selected_metadata::Status::Complete {
            break;
        }
    }
    cursor.finish(now(clock)?).map_err(mime)?;
    worker.append(b"}")
}

/// Trusted response owner: checks original admission/deadline and retains
/// bounded provisional bytes. Implementations must not publish partial output.
pub trait Output {
    fn admit(&mut self) -> Result<(), Error> {
        self.check()
    }
    fn check(&mut self) -> Result<(), Error>;
    fn append(&mut self, bytes: &[u8]) -> Result<(), Error>;
}
impl Output for Worker<'_, '_, '_, '_, '_> {
    fn admit(&mut self) -> Result<(), Error> {
        Worker::check(self)
    }
    fn check(&mut self) -> Result<(), Error> {
        if self
            .retention
            .observe_before(self.execution)
            .map_err(storage)?
        {
            Ok(())
        } else {
            Err(Error::ExecutionDeadline)
        }
    }
    fn append(&mut self, bytes: &[u8]) -> Result<(), Error> {
        Worker::append(self, bytes)
    }
}
struct Buffered<'a, T: ?Sized> {
    inner: &'a mut T,
    bytes: [u8; 4096],
    used: usize,
}
impl<T: Output + ?Sized> Buffered<'_, T> {
    fn flush(&mut self) -> Result<(), Error> {
        if self.used != 0 {
            self.inner
                .append(self.bytes.get(..self.used).ok_or(Error::State)?)?;
            self.used = 0;
        }
        self.inner.check()
    }
}
impl<T: Output + ?Sized> Output for Buffered<'_, T> {
    fn check(&mut self) -> Result<(), Error> {
        self.inner.check()
    }
    fn append(&mut self, mut bytes: &[u8]) -> Result<(), Error> {
        self.check()?;
        while !bytes.is_empty() {
            let n = bytes.len().min(self.bytes.len() - self.used);
            self.bytes
                .get_mut(self.used..self.used + n)
                .ok_or(Error::State)?
                .copy_from_slice(bytes.get(..n).ok_or(Error::State)?);
            self.used += n;
            bytes = bytes.get(n..).ok_or(Error::State)?;
            if self.used == self.bytes.len() {
                self.flush()?;
            }
        }
        Ok(())
    }
}
const _: () = assert!(
    std::mem::size_of::<Part>()
        + std::mem::size_of::<Option<crate::root_headers::View<'_>>>()
        + std::mem::size_of::<Node>()
        + std::mem::size_of::<crate::blob_value_parts::Fact>()
        + std::mem::size_of::<Candidate>()
        + 12
        <= 512
);

const _: () =
    assert!(std::mem::size_of::<[Option<crate::header_property::Property<'static>>; 64]>() <= 4096);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod allocation_tests {
    use super::*;
    const MESSAGE: &[u8] = b"From: Sender <sender@example.test>\r\nSubject: Budgeted projection\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: multipart/alternative; boundary=y\r\n\r\n--y\r\nContent-Type: text/plain\r\n\r\nHarvest & rain\r\n--y\r\nContent-Type: text/html\r\n\r\n<p>Harvest &amp; rain</p>\r\n--y--\r\n--x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=report.bin\r\n\r\nattachment\r\n--x--\r\n";
    struct Fixed;
    impl Clock for Fixed {
        fn sample(&self) -> Result<crate::ports::Time, crate::ports::Error> {
            Ok(crate::ports::Time {
                utc_ms: 0,
                monotonic: Tick(1),
            })
        }
    }
    fn work() -> Meter {
        Meter::new(
            td_mime::time::Deadline::at(Tick(99)),
            crate::admission::work::Charge {
                records: 100_000_000,
                io_bytes: 100_000_000,
                output_bytes: 100_000_000,
                unlinks: 0,
            },
        )
    }
    struct Observe<'a> {
        memory: &'a td_alloc::Budget,
        peak: usize,
        checks: usize,
        cancel: usize,
        bytes: Vec<u8>,
    }
    impl Output for Observe<'_> {
        fn check(&mut self) -> Result<(), Error> {
            self.peak = self.peak.max(self.memory.used());
            self.checks += 1;
            if self.checks >= self.cancel {
                Err(Error::ExecutionDeadline)
            } else {
                Ok(())
            }
        }
        fn append(&mut self, bytes: &[u8]) -> Result<(), Error> {
            self.check()?;
            self.bytes.extend_from_slice(bytes);
            Ok(())
        }
    }
    fn selection(memory: &td_alloc::Budget) -> Selection {
        let mut selected = Selection::new(memory).unwrap();
        for name in [
            "subject",
            "from",
            "preview",
            "textBody",
            "htmlBody",
            "attachments",
            "bodyValues",
        ] {
            selected.include(name, &mut work(), &Fixed).unwrap();
        }
        selected
    }
    fn run(
        memory: &td_alloc::Budget,
        selected: &Selection,
        attempts: usize,
        cancel: usize,
    ) -> (Result<(), Error>, usize, usize) {
        let mut output = Observe {
            memory,
            peak: memory.used(),
            checks: 0,
            cancel,
            bytes: Vec::new(),
        };
        let mut result = Err(Error::State);
        crate::store_fs::with_pinned_fixture(MESSAGE, &Fixed, |mut pin| {
            result = td_alloc::with_reservation_limit(attempts, || {
                write_verified(
                    &mut output,
                    &mut pin,
                    &Limits::default(),
                    (
                        selected,
                        Options {
                            fetch_all: true,
                            ..Options::default()
                        },
                    ),
                    &Fixed,
                    &mut work(),
                )
            });
        });
        if result.is_ok() {
            let json = format!(
                "{{\"id\":null{}}}",
                std::str::from_utf8(&output.bytes).unwrap()
            );
            let json = td_json::parse(&json).unwrap();
            assert_eq!(json["subject"].as_str(), Some("Budgeted projection"));
            assert_eq!(json["textBody"].as_array().unwrap().len(), 1);
            assert_eq!(json["htmlBody"].as_array().unwrap().len(), 1);
            assert_eq!(json["attachments"].as_array().unwrap().len(), 1);
            assert_eq!(json["bodyValues"].as_object().unwrap().len(), 2);
            assert!(json["preview"].as_str().unwrap().contains("Harvest"));
        }
        (result, output.peak, output.checks)
    }
    #[test]
    fn projection_allocation_refusals_release_scratch_and_recover() {
        let parent = td_alloc::Budget::new(4 * 1024 * 1024).unwrap();
        let memory = parent.child(2 * 1024 * 1024).unwrap();
        let selected = selection(&memory);
        let retained = memory.used();
        let mut succeeded = None;
        for attempts in 0..32 {
            let (result, _, _) = run(&memory, &selected, attempts, usize::MAX);
            assert_eq!(memory.used(), retained, "attempts={attempts}");
            if result.is_ok() {
                succeeded = Some(attempts);
                break;
            }
            assert_eq!(result, Err(Error::ResponseCapacity), "attempts={attempts}");
        }
        assert!(
            succeeded.is_some_and(|n| n >= 10),
            "all MIME backing must take fallible admission: {succeeded:?}"
        );
        let (result, peak, _) = run(&memory, &selected, usize::MAX, usize::MAX);
        assert_eq!(result, Ok(()));
        assert!(peak > retained + 300_000);
        assert!(peak < memory.limit());
        eprintln!("projection nested fixture peak={peak} retained={retained}");
        drop(selected);
        assert_eq!(memory.used(), td_alloc::Budget::metadata_bytes());
        drop(memory);
        assert_eq!(parent.used(), td_alloc::Budget::metadata_bytes());
    }
    #[test]
    fn projection_byte_exhaustion_and_late_cancellation_release_child_storage() {
        let parent = td_alloc::Budget::new(4 * 1024 * 1024).unwrap();
        let memory = parent.child(2 * 1024 * 1024).unwrap();
        let selected = selection(&memory);
        let retained = memory.used();
        let (result, peak, checks) = run(&memory, &selected, usize::MAX, usize::MAX);
        assert_eq!(result, Ok(()));
        let occupied = memory
            .charge(memory.limit() - retained - 128 * 1024)
            .unwrap();
        assert_eq!(
            run(&memory, &selected, usize::MAX, usize::MAX).0,
            Err(Error::ResponseCapacity)
        );
        assert_eq!(memory.used(), retained + occupied.bytes());
        drop(occupied);
        for cancel in [1, checks / 2, checks - 1] {
            let (result, _, _) = run(&memory, &selected, usize::MAX, cancel);
            assert_eq!(result, Err(Error::ExecutionDeadline));
            assert_eq!(memory.used(), retained);
        }
        assert_eq!(run(&memory, &selected, usize::MAX, usize::MAX).0, Ok(()));
        assert_eq!(memory.used(), retained);
        assert!(peak < 2 * 1024 * 1024);
    }
    #[test]
    fn maximum_default_parts_fit_the_original_method_allowance() {
        let limits = Limits::default();
        let memory = td_alloc::Budget::new(limits.jmap_method_bytes).unwrap();
        let selected = selection(&memory);
        let retained = memory.used();
        let mut source = String::from(
            "Subject: Many parts\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n",
        );
        for _ in 1..limits.mime_parts {
            source.push_str("--x\r\nContent-Type: text/plain\r\n\r\nbody\r\n");
        }
        source.push_str("--x--\r\n");
        let mut output = Observe {
            memory: &memory,
            peak: retained,
            checks: 0,
            cancel: usize::MAX,
            bytes: Vec::new(),
        };
        crate::store_fs::with_pinned_fixture(source.as_bytes(), &Fixed, |mut pin| {
            write_verified(
                &mut output,
                &mut pin,
                &limits,
                (
                    &selected,
                    Options {
                        fetch_all: true,
                        ..Options::default()
                    },
                ),
                &Fixed,
                &mut work(),
            )
            .unwrap();
        });
        assert_eq!(memory.used(), retained);
        assert!(output.peak < limits.jmap_method_bytes);
        eprintln!(
            "projection maximum parts={} peak={}",
            limits.mime_parts, output.peak
        );
    }
    #[test]
    fn selection_growth_refusal_keeps_prior_names_and_duplicate_needs_no_allocation() {
        let memory = td_alloc::Budget::new(64 * 1024).unwrap();
        let mut selected = Selection::new(&memory).unwrap();
        for n in 0..8 {
            selected
                .include(&format!("header:X-{n}:asText"), &mut work(), &Fixed)
                .unwrap();
        }
        let retained = memory.used();
        td_alloc::with_reservation_limit(0, || {
            assert_eq!(
                selected.include("header:X-8:asText", &mut work(), &Fixed),
                Err(Error::ResponseCapacity)
            );
            selected
                .include("header:X-0:asText", &mut work(), &Fixed)
                .unwrap();
        });
        assert_eq!(selected.headers.len(), 8);
        assert_eq!(memory.used(), retained);
        selected
            .include("header:X-8:asText", &mut work(), &Fixed)
            .unwrap();
        assert_eq!(selected.headers.len(), 9);
        drop(selected);
        assert_eq!(memory.used(), td_alloc::Budget::metadata_bytes());
    }
}
