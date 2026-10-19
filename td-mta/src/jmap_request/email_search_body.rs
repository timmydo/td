//! Search the existing decoded header and MIME sources without retaining bodies.
use super::*;
use crate::jmap_request::dispatch::email_search::{charge, Search};

struct Headers<'a> {
    bytes: Vec<u8>,
    maximum: usize,
    clock: &'a dyn Clock,
    deadline: td_mime::time::Deadline,
    last: Tick,
}
impl Output for Headers<'_> {
    fn check(&mut self) -> Result<(), Error> {
        let tick = now(self.clock)?;
        if tick < self.last || self.deadline.expired(tick) {
            return Err(Error::ExecutionDeadline);
        }
        self.last = tick;
        Ok(())
    }
    fn append(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.check()?;
        if bytes.len() > self.maximum.saturating_sub(self.bytes.len()) {
            return Err(Error::StorageCapacity);
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|_| Error::StorageCapacity)?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
}
pub(in crate::jmap_request::dispatch) fn search_verified(
    pin: &mut PinnedBlob<'_, '_>,
    limits: &Limits,
    search: &mut Search,
    clock: &dyn Clock,
    work: &mut Meter,
) -> Result<bool, Error> {
    let result = search_inner(pin, limits, search, clock, work, &mut HeaderBudget::new());
    pin.check_deadline().map_err(storage)?;
    if let Some(stop) = work.stopped() {
        return Err(mime(stop));
    }
    charge(work, clock, 0)?;
    result
}
fn search_inner(
    pin: &mut PinnedBlob<'_, '_>,
    limits: &Limits,
    search: &mut Search,
    clock: &dyn Clock,
    work: &mut Meter,
    budget: &mut HeaderBudget,
) -> Result<bool, Error> {
    search.reset();
    pin.check_deadline().map_err(storage)?;
    if pin.len() > limits.message_bytes as u64 {
        return Err(Error::StorageCapacity);
    }
    if limits.mime_parts == 0 {
        return Err(Error::MessageInterpretation);
    }
    let mut pending = Vec::new();
    pending
        .try_reserve_exact(limits.mime_parts)
        .map_err(|_| Error::StorageCapacity)?;
    pending.push(Context {
        extent: 0..pin.len(),
        stage: 0,
    });
    let mut traversal = Traversal {
        search,
        budget,
        pending,
        parts: limits.mime_parts,
        headers: limits.header_bytes,
    };
    while let Some(context) = traversal.pending.pop() {
        if traversal.parts == 0 {
            return Err(Error::MessageInterpretation);
        }
        let mut local = *limits;
        local.mime_parts = traversal.parts;
        local.mime_depth = limits.mime_depth.min(traversal.parts);
        // The parser requires a positive ceiling. Aggregate accounting below
        // still refuses any header bytes beyond the remaining allowance.
        local.header_bytes = traversal.headers.max(1);
        search_context(pin, &local, clock, work, &mut traversal, context)?;
    }
    Ok(traversal.search.matched())
}
struct Context {
    extent: std::ops::Range<u64>,
    stage: usize,
}
struct Traversal<'a> {
    search: &'a mut Search,
    budget: &'a mut HeaderBudget,
    pending: Vec<Context>,
    parts: usize,
    headers: usize,
}
fn search_context(
    pin: &mut PinnedBlob<'_, '_>,
    limits: &Limits,
    clock: &dyn Clock,
    work: &mut Meter,
    traversal: &mut Traversal<'_>,
    context: Context,
) -> Result<(), Error> {
    let search = &mut *traversal.search;
    let budget = &mut *traversal.budget;
    let mut selected = Selection::new();
    for name in ["From", "To", "Cc", "Bcc"] {
        selected.include(&format!("header:{name}:asAddresses:all"), work, clock)?;
    }
    selected.include("header:Subject:asText:all", work, clock)?;
    let mut arena = buffer(
        limits
            .header_bytes
            .checked_add(limits.mime_parts)
            .and_then(|n| n.checked_add(2))
            .ok_or(Error::StorageCapacity)?,
    )?;
    let mut scratch = Scratch::new();
    let mut output = Headers {
        bytes: Vec::new(),
        maximum: limits
            .header_bytes
            .checked_mul(6)
            .and_then(|n| n.checked_add(4096))
            .ok_or(Error::StorageCapacity)?,
        clock,
        deadline: work.deadline(),
        last: now(clock)?,
    };
    output.append(b"{\"_\":null")?;
    write_headers(
        &mut output,
        pin,
        (limits, &selected),
        clock,
        work,
        HeaderBacking {
            extent: context.extent.clone(),
            arena: &mut arena,
            budget,
            scratch: &mut scratch,
        },
    )?;
    output.append(b"}")?;
    charge(work, clock, output.bytes.len() as u64)?;
    let headers = td_json::parse(std::str::from_utf8(&output.bytes).map_err(|_| Error::State)?)
        .map_err(|_| Error::MessageInterpretation)?;
    for (_, values) in headers.as_object().ok_or(Error::State)?.iter() {
        if values.is_null() {
            continue;
        }
        for value in values.as_array().ok_or(Error::State)? {
            if let Some(text) = value.as_str() {
                search.text(text, work, clock)?;
            } else {
                for address in value.as_array().ok_or(Error::State)? {
                    for name in ["name", "email"] {
                        if let Some(text) = address.get(name).and_then(td_json::Json::as_str) {
                            search.text(text, work, clock)?;
                        }
                    }
                }
            }
        }
    }
    drop(headers);
    drop(output);

    let mut chunk = [0; 4096];
    let mut parts = buffer(limits.mime_parts)?;
    let mut headers = buffer(limits.mime_parts)?;
    let mut cursor = blob_structure::Cursor::new(
        pin,
        blob_structure::Settings {
            extent: context.extent,
            limits: mime_structure::Limits {
                header_bytes: limits.header_bytes,
                mime_depth: limits.mime_depth,
                mime_parts: limits.mime_parts,
            },
        },
        blob_structure::Buffers {
            header_bytes: &mut arena,
            body: &mut chunk,
            parts: &mut parts,
            headers: &mut headers,
        },
        work,
        budget,
        now(clock)?,
    )
    .map_err(mime)?;
    while cursor.poll(now(clock)?).map_err(mime)? != mime_structure::Status::Complete {}
    let mut original = cursor.finish(now(clock)?).map_err(mime)?;
    traversal.parts = traversal
        .parts
        .checked_sub(original.parts.len())
        .ok_or(Error::MessageInterpretation)?;
    traversal.headers = traversal
        .headers
        .checked_sub(
            usize::try_from(original.header_bytes).map_err(|_| Error::MessageInterpretation)?,
        )
        .ok_or(Error::MessageInterpretation)?;
    let mut metadata = Metadata::new(limits.header_bytes.min(16 * 1024))?;
    let mut input = buffer(crate::mime_input::INPUT_BYTES)?;
    let mut checkpoints = crate::mime_input::Checkpoints::default();
    for index in 0..original.parts.len() {
        original.source.check_deadline().map_err(storage)?;
        let mut cursor = crate::blob_part::Cursor::new(
            reborrow(&mut original),
            u16::try_from(index + 1).map_err(|_| Error::State)?,
            metadata.backing(),
            &mut scratch,
            now(clock)?,
        )
        .map_err(mime)?;
        while cursor.poll(now(clock)?).map_err(mime)? != mime_part_headers::Status::Complete {}
        let part = cursor.finish(now(clock)?).map_err(mime)?;
        let heads = &part.metadata.metadata.headers;
        if let Some(filename) = heads.filename {
            search.text(
                std::str::from_utf8(filename).map_err(|_| Error::State)?,
                part.structure.work,
                clock,
            )?;
        }
        if heads.content_type.eq_ignore_ascii_case(b"message/rfc822") {
            let node = part.metadata.part;
            if node.encoding != mime_structure::Encoding::Identity
                || node.problems() & mime_structure::UNKNOWN_ENCODING != 0
                || context.stage == 6
                || traversal.pending.len() >= traversal.parts
            {
                return Err(Error::MessageInterpretation);
            }
            charge(part.structure.work, clock, 1)?;
            traversal.pending.push(Context {
                extent: node.body_start..node.entity_end,
                stage: context.stage + 1,
            });
            continue;
        }
        // Other message types require their own decoded source context.
        if heads
            .content_type
            .get(..8)
            .is_some_and(|p| p.eq_ignore_ascii_case(b"message/"))
        {
            return Err(Error::MessageInterpretation);
        }
        if !heads
            .content_type
            .get(..5)
            .is_some_and(|p| p.eq_ignore_ascii_case(b"text/"))
        {
            continue;
        }
        let spec = part.text_spec().map_err(|_| Error::MessageInterpretation)?;
        search.field();
        let mut reader = crate::mime_text::Reader::new_typed(
            crate::mime_text::InputSource {
                source: part.structure.source,
                offset: spec.offset,
                length: spec.length,
                encoding: spec.encoding,
                charset: spec.charset,
            },
            &mut input,
            &mut checkpoints,
        )
        .map_err(mime)?;
        let clock = crate::mime_text::CheckedClock::new(clock, now(clock)?);
        let mut html = spec.html.then(td_html::extract::Extractor::new);
        let (mut scalar, mut ended) = (None, false);
        loop {
            reader.source_mut().check_deadline().map_err(storage)?;
            if scalar.is_none() && !ended {
                match reader.poll(&clock, part.structure.work).map_err(mime)? {
                    crate::mime_text::Status::Scalar(c) => scalar = Some(c),
                    crate::mime_text::Status::Complete => ended = true,
                    crate::mime_text::Status::Yield => continue,
                }
            }
            if let Some(html) = &mut html {
                let progress =
                    crate::html_text::poll_shared(html, scalar, ended, part.structure.work, &clock)
                        .map_err(|error| match error {
                            crate::html_text::Error::Work(stop) => mime(stop),
                            _ => Error::MessageInterpretation,
                        })?;
                if progress.consumed {
                    scalar = None;
                }
                if let Some(c) = progress.scalar {
                    search.scalar(c, part.structure.work, &clock)?;
                }
                if progress.status == crate::html_text::Status::Complete {
                    break;
                }
            } else {
                if let Some(c) = scalar.take() {
                    search.scalar(c, part.structure.work, &clock)?;
                }
                if ended {
                    break;
                }
            }
        }
        reader.source_mut().check_deadline().map_err(storage)?;
        reader.release().map_err(mime)?;
    }
    original.source.check_deadline().map_err(storage)?;
    charge(original.work, clock, 0)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::admission::work::Charge;
    const MESSAGE: &[u8] = b"From: =?utf-8?Q?Gard=C3=A9ner?= <sender@example.test>\r\nSubject: Orchard\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<style>hidden</style><p>Harvest &amp; rain</p>\r\n--x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=report.bin\r\n\r\nnotsearchable\r\n--x--\r\n";
    struct Fixed;
    impl Clock for Fixed {
        fn sample(&self) -> Result<crate::ports::Time, crate::ports::Error> {
            Ok(crate::ports::Time {
                utc_ms: 0,
                monotonic: Tick(1),
            })
        }
    }
    fn work(records: u64) -> Meter {
        Meter::new(
            td_mime::time::Deadline::at(Tick(99)),
            Charge {
                records,
                io_bytes: 100_000_000,
                output_bytes: 100_000_000,
                unlinks: 0,
            },
        )
    }
    fn attached(body: &str) -> String {
        format!("Content-Type: message/rfc822\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{body}")
    }
    fn check_source(message: &[u8], limits: &Limits, query: &str) -> Result<bool, Error> {
        let mut outcome = Err(Error::State);
        crate::store_fs::with_pinned_fixture(message, &Fixed, |mut pin| {
            outcome = search_verified(
                &mut pin,
                limits,
                &mut Search::parse(query).unwrap(),
                &Fixed,
                &mut work(100_000_000),
            );
        });
        outcome
    }
    #[test]
    fn attached_identity_messages_search_headers_and_text_as_separate_fields() {
        let inner = "From: Inner <inner@example.test>\r\nSubject: =?utf-8?Q?Orchard_caf=C3=A9?=\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<style>hiddenneedle</style><p>Harvest &amp; rain</p>";
        let source = attached(&attached(inner));
        for (query, expected) in [
            ("inner@example.test CAFÉ harvest", true),
            ("\"Harvest & rain\"", true),
            ("hiddenneedle", false),
            ("\"café Harvest\"", false),
            ("absent", false),
        ] {
            assert_eq!(
                check_source(source.as_bytes(), &Limits::default(), query),
                Ok(expected),
                "{query}"
            );
        }
        let siblings = format!(
            "Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\n{}\r\n--x\r\n{}\r\n--x--\r\n",
            attached("Subject: First\r\n\r\napples"),
            attached("Subject: Second\r\n\r\npears")
        );
        assert_eq!(
            check_source(siblings.as_bytes(), &Limits::default(), "first pears"),
            Ok(true)
        );
    }
    #[test]
    fn attached_headerless_message_fits_exact_header_allowance() {
        let header = "Content-Type: message/rfc822\r\n";
        let limits = Limits {
            header_bytes: header.len(),
            mime_depth: 1,
            mime_parts: 2,
            ..Limits::default()
        };
        let source = format!("{header}\r\n\r\nneedle");
        assert_eq!(check_source(source.as_bytes(), &limits, "needle"), Ok(true));
        let source = format!("{header}\r\nX: y\r\n\r\nneedle");
        assert_eq!(
            check_source(source.as_bytes(), &limits, "needle"),
            Err(Error::MessageInterpretation)
        );
    }
    #[test]
    fn embedded_contexts_keep_aggregate_limits_and_refuse_unsupported_sources() {
        let inner = "Subject: Found\r\n\r\nbody";
        let mut source = inner.to_owned();
        for _ in 0..6 {
            source = attached(&source);
        }
        assert_eq!(
            check_source(source.as_bytes(), &Limits::default(), "found"),
            Ok(true)
        );
        source = attached(&source);
        assert_eq!(
            check_source(source.as_bytes(), &Limits::default(), "found"),
            Err(Error::MessageInterpretation)
        );
        let source = attached(inner);
        let limits = Limits {
            mime_parts: 1,
            ..Limits::default()
        };
        assert_eq!(
            check_source(source.as_bytes(), &limits, "found"),
            Err(Error::MessageInterpretation)
        );
        let limits = Limits {
            mime_parts: 2,
            mime_depth: 2,
            ..Limits::default()
        };
        assert_eq!(check_source(source.as_bytes(), &limits, "found"), Ok(true));
        let headers = b"Content-Type: message/rfc822\r\nContent-Transfer-Encoding: 8bit\r\nSubject: Found\r\n";
        let limits = Limits {
            header_bytes: headers.len(),
            ..Limits::default()
        };
        assert_eq!(check_source(source.as_bytes(), &limits, "found"), Ok(true));
        // Each context fits alone, but their sum exceeds this bound by one.
        let limits = Limits {
            header_bytes: headers.len() - 1,
            ..limits
        };
        assert_eq!(
            check_source(source.as_bytes(), &limits, "found"),
            Err(Error::MessageInterpretation)
        );
        for header in [
            "Content-Type: message/rfc822\r\nContent-Transfer-Encoding: base64",
            "Content-Type: message/rfc822\r\nContent-Transfer-Encoding: quoted-printable",
            "Content-Type: message/rfc822\r\nContent-Transfer-Encoding: unknown",
            "Content-Type: message/global",
        ] {
            let source = format!("Subject: Found\r\n{header}\r\n\r\n{inner}");
            assert_eq!(
                check_source(source.as_bytes(), &Limits::default(), "found"),
                Err(Error::MessageInterpretation),
                "{header}"
            );
        }
    }
    #[test]
    fn verified_sources_decode_names_extract_html_and_exclude_binary_data() {
        crate::store_fs::with_pinned_fixture(MESSAGE, &Fixed, |mut pin| {
            for (query, expected) in [
                ("gardéner sender@example.test harvest report.bin", true),
                ("\"Harvest & rain\"", true),
                ("hidden", false),
                ("notsearchable", false),
            ] {
                assert_eq!(
                    search_verified(
                        &mut pin,
                        &Limits::default(),
                        &mut Search::parse(query).unwrap(),
                        &Fixed,
                        &mut work(100_000_000)
                    ),
                    Ok(expected),
                    "{query}"
                );
            }
        });
    }
    #[test]
    fn original_header_allowance_spans_headers_and_part_metadata() {
        crate::store_fs::with_pinned_fixture(MESSAGE, &Fixed, |mut pin| {
            let limits = Limits::default();
            let mut budget = HeaderBudget::new();
            let initial = budget.steps_remaining();
            assert!(search_inner(
                &mut pin,
                &limits,
                &mut Search::parse("harvest").unwrap(),
                &Fixed,
                &mut work(100_000_000),
                &mut budget
            )
            .unwrap());
            let whole = initial - budget.steps_remaining();

            let mut selected = Selection::new();
            let mut meter = work(100_000_000);
            for name in ["From", "To", "Cc", "Bcc"] {
                selected
                    .include(
                        &format!("header:{name}:asAddresses:all"),
                        &mut meter,
                        &Fixed,
                    )
                    .unwrap();
            }
            selected
                .include("header:Subject:asText:all", &mut meter, &Fixed)
                .unwrap();
            let mut budget = HeaderBudget::new();
            let mut arena = vec![0; limits.header_bytes + limits.mime_parts + 2];
            let mut output = Headers {
                bytes: Vec::new(),
                maximum: 1_000_000,
                clock: &Fixed,
                deadline: meter.deadline(),
                last: Tick(1),
            };
            write_headers(
                &mut output,
                &mut pin,
                (&limits, &selected),
                &Fixed,
                &mut meter,
                HeaderBacking {
                    extent: 0..MESSAGE.len() as u64,
                    arena: &mut arena,
                    budget: &mut budget,
                    scratch: &mut Scratch::new(),
                },
            )
            .unwrap();
            let headers = initial - budget.steps_remaining();
            let allowance = whole - 1;
            assert!(headers < allowance && whole - headers < allowance);
            let mut budget = HeaderBudget::new();
            budget
                .charge(
                    &mut meter,
                    Tick(1),
                    0,
                    initial - allowance,
                    &mut crate::nfc::Credit::new(),
                )
                .unwrap();
            assert_eq!(
                search_inner(
                    &mut pin,
                    &limits,
                    &mut Search::parse("harvest").unwrap(),
                    &Fixed,
                    &mut meter,
                    &mut budget
                ),
                Err(Error::MessageInterpretation)
            );
            assert_eq!(
                meter.stopped(),
                None,
                "the shared interpretation cap, not job work, refused"
            );
        });
    }
    #[test]
    fn metadata_work_exhaustion_keeps_its_resource_error() {
        crate::store_fs::with_pinned_fixture(MESSAGE, &Fixed, |mut pin| {
            let mut baseline = work(100_000_000);
            search_verified(
                &mut pin,
                &Limits::default(),
                &mut Search::parse("harvest").unwrap(),
                &Fixed,
                &mut baseline,
            )
            .unwrap();
            let used = 100_000_000 - baseline.remaining().records;
            let mut reached_metadata = false;
            for available in (0..used).step_by((used / 32).max(1) as usize) {
                let mut meter = work(available);
                let raw = search_inner(
                    &mut pin,
                    &Limits::default(),
                    &mut Search::parse("harvest").unwrap(),
                    &Fixed,
                    &mut meter,
                    &mut HeaderBudget::new(),
                );
                if raw == Err(Error::MessageInterpretation) && meter.stopped().is_some() {
                    reached_metadata = true;
                    assert_eq!(
                        search_verified(
                            &mut pin,
                            &Limits::default(),
                            &mut Search::parse("harvest").unwrap(),
                            &Fixed,
                            &mut work(available)
                        ),
                        Err(Error::StorageCapacity)
                    );
                    break;
                }
            }
            assert!(
                reached_metadata,
                "exercise nested metadata admission failure"
            );
        });
    }
}
