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
    ports::{BlobReader, Clock, Tick},
    store_fs::PinnedBlob,
};
pub(in crate::jmap_request::dispatch) use search::search_verified;

pub(super) fn buffer<T: Default + Clone>(count: usize) -> Result<Vec<T>, Error> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(count)
        .map_err(|_| Error::ResponseCapacity)?;
    result.resize(count, T::default());
    Ok(result)
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

struct Metadata {
    heads: Vec<u8>,
    charset: Vec<u8>,
    filename: Vec<u8>,
    id: Vec<u8>,
    language: Vec<u8>,
    location: Vec<u8>,
}
impl Metadata {
    fn new(limit: usize) -> Result<Self, Error> {
        let json = limit;
        Ok(Self {
            heads: buffer(limit.checked_add(14).ok_or(Error::ResponseCapacity)?)?,
            charset: buffer(limit)?,
            filename: buffer(json)?,
            id: buffer(json)?,
            language: buffer(json)?,
            location: buffer(json)?,
        })
    }
    fn backing(&mut self) -> label_json::Backing<'_> {
        label_json::Backing {
            headers: mime_part_headers::Backing {
                heads: &mut self.heads,
                charset: &mut self.charset,
                filename: &mut self.filename,
            },
            labels: crate::mime_label_fields::json::Backing {
                content_id: &mut self.id,
                content_language: &mut self.language,
            },
            content_location: &mut self.location,
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
    let result = write_verified_inner(output, pin, limits, selection, clock, work);
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
    settings: (&Limits, &Selection),
    clock: &dyn Clock,
    work: &mut Meter,
    backing: HeaderBacking<'_>,
) -> Result<(), Error> {
    let (limits, selected) = settings;
    let mut chunk = [0; 4096];
    if !selected.headers.is_empty() {
        let mut properties = Vec::new();
        properties
            .try_reserve_exact(selected.headers.len())
            .map_err(|_| Error::ResponseCapacity)?;
        for name in &selected.headers {
            let name = std::str::from_utf8(name.bytes()).map_err(|_| Error::State)?;
            let mut parser =
                crate::header_property::Cursor::new(name, crate::header_property::Context::Email);
            loop {
                if let crate::header_property::Status::Complete(property) =
                    parser.poll(now(clock)?, work).map_err(mime)?
                {
                    properties.push(property.ok_or(Error::State)?);
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
        for property in properties {
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
) -> Result<(), Error> {
    output.check()?;
    pin.check_deadline().map_err(storage)?;
    if pin.len() > crate::postgres_body::MAX_BODY_BYTES {
        return Err(Error::StorageCapacity);
    }
    let (selected, options) = selection;
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
    let mut arena = buffer(arena_bytes)?;
    let extent = 0..pin.len();
    write_headers(
        worker,
        pin,
        (limits, selected),
        clock,
        work,
        HeaderBacking {
            extent,
            arena: &mut arena,
            budget: &mut budget,
            scratch: &mut scratch,
        },
    )?;
    if !selected.body() {
        pin.check_deadline().map_err(storage)?;
        worker.flush()?;
        return pin.check_deadline().map_err(storage);
    }
    let parse_limits = mime_structure::Limits {
        header_bytes: limits.header_bytes,
        mime_depth: limits.mime_depth,
        mime_parts: limits.mime_parts,
    };
    let mut parts: Vec<Part> = buffer(limits.mime_parts)?;
    let mut headers = buffer(limits.mime_parts)?;
    let length = pin.len();
    let mut cursor = blob_structure::Cursor::new(
        pin,
        blob_structure::Settings {
            extent: 0..length,
            limits: parse_limits,
        },
        blob_structure::Buffers {
            header_bytes: &mut arena,
            body: &mut chunk,
            parts: &mut parts,
            headers: &mut headers,
        },
        work,
        &mut budget,
        now(clock)?,
    )
    .map_err(mime)?;
    while cursor.poll_batch(clock).map_err(mime)? != mime_structure::Status::Complete {
        worker.check()?;
    }
    let mut original = cursor.finish(now(clock)?).map_err(mime)?;
    let count = original.parts.len();
    let metadata_limit = limits.header_bytes.min(16 * 1024);
    let mut metadata = Metadata::new(metadata_limit)?;
    let mut nodes: Vec<Node> = buffer(count)?;
    let mut facts = Vec::new();
    facts
        .try_reserve_exact(count)
        .map_err(|_| Error::ResponseCapacity)?;
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
        facts.push(crate::blob_value_parts::Fact {
            end: End {
                part: part.metadata.part,
                node: part.metadata.node,
            },
            kind: crate::blob_value_parts::Kind::from_headers(&part.metadata.metadata.headers),
        });
    }
    let mut candidates: Vec<Candidate> = buffer(count)?;
    let mut locators =
        crate::blob_part_locators::Cursor::new(original, &mut candidates, now(clock)?)
            .map_err(mime)?;
    while locators.poll(now(clock)?).map_err(mime)? != crate::blob_part_locators::Status::Complete {
        worker.check()?;
    }
    let original = locators.finish(now(clock)?).map_err(mime)?.structure;
    let (mut text, mut html, mut attachments, mut membership) = (
        buffer(count)?,
        buffer(count)?,
        buffer(count)?,
        buffer(count)?,
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
        fragment: buffer(192 * 1024)?,
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
    let mut decode_input = buffer(crate::mime_input::INPUT_BYTES)?;
    let mut checkpoints = crate::mime_input::Checkpoints::default();
    if selected.preview {
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
    }
    if selected.values {
        let original = crate::blob_body_lists::Retained {
            structure: reborrow(&mut original.structure),
            nodes: original.nodes,
            lists: original.lists,
        };
        let mut ordinals = buffer(count)?;
        let mut marks = buffer(count)?;
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
    fragment: Vec<u8>,
}
fn write_part(
    worker: &mut impl Output,
    original: &mut blob_structure::Retained<'_, '_, '_, '_, '_, '_>,
    ordinal: u16,
    metadata: &mut Metadata,
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
