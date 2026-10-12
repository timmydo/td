#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::work::{Charge, Meter},
    mime_body_lists::{Disposition, Media},
    mime_structure,
    ports::{Clock, Deadline, Time},
};
use std::sync::atomic::{AtomicU64, Ordering};
struct Timer {
    calls: AtomicU64,
    expires: AtomicU64,
}
impl Timer {
    fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
            expires: AtomicU64::new(u64::MAX),
        }
    }
    fn after(&self, n: u64) {
        self.expires
            .store(self.calls.load(Ordering::Relaxed) + n, Ordering::Relaxed);
    }
}
impl Clock for Timer {
    fn sample(&self) -> Result<Time, PolicyError> {
        let n = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(Time {
            monotonic: Tick(if n >= self.expires.load(Ordering::Relaxed) {
                100
            } else {
                1
            }),
            utc_ms: 0,
        })
    }
}
struct Storage {
    heads: [u8; 256],
    charset: [u8; 256],
    filename: [u8; 256],
    id: [u8; 256],
    language: [u8; 256],
    location: [u8; 256],
}
impl Storage {
    fn new() -> Self {
        Self {
            heads: [0; 256],
            charset: [0; 256],
            filename: [0; 256],
            id: [0; 256],
            language: [0; 256],
            location: [0; 256],
        }
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
fn meter() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10_000_000,
            records: 10_000_000,
            output_bytes: 10_000_000,
            unlinks: 0,
        },
    )
}
fn settings(length: usize) -> blob_structure::Settings {
    blob_structure::Settings {
        extent: 0..length as u64,
        limits: mime_structure::Limits {
            header_bytes: 1024,
            mime_depth: 4,
            mime_parts: 8,
        },
    }
}
fn structure<'p, 'v, 'c, 'b, 'r, 'o, 'w>(
    source: &'p mut PinnedBlob<'v, 'c>,
    length: usize,
    buffers: blob_structure::Buffers<'b, 'r, 'o>,
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
) -> blob_structure::Retained<'p, 'v, 'c, 'b, 'o, 'w> {
    let mut cursor =
        blob_structure::Cursor::new(source, settings(length), buffers, work, budget, Tick(1))
            .unwrap();
    for _ in 0..100_000 {
        if cursor.poll(Tick(1)).unwrap() == mime_structure::Status::Complete {
            return cursor.finish(Tick(1)).unwrap();
        }
    }
    panic!("traversal did not finish");
}
fn drain(cursor: &mut Cursor<'_, '_, '_, '_, '_, '_>) -> Result<usize, Error> {
    for n in 1..100_000 {
        if cursor.poll(Tick(1))? == Status::Complete {
            return Ok(n);
        }
    }
    panic!("metadata did not finish");
}
#[test]
fn retained_prefix_metadata_matches_resident_and_returns_original_owners() {
    let bytes = concat!("Subject: root\r\nContent-Type: multipart/digest;boundary=x\r\n\r\n--x\r\n",
        "Content-ID: <child@example.org>\r\nContent-Language: en, fr\r\nContent-Location: ../child\r\n\r\nFrom: hidden@example.org\r\n\r\nbody\r\n--x\r\n",
        "Content-Type: text/html;charset=UTF-8\r\nContent-Disposition: attachment; filename*=utf-8''caf%C3%A9.html\r\n\r\n<b>text</b>\r\n--x--\r\n").as_bytes();
    let timer = Timer::new();
    crate::store_fs::with_pinned_fixture(bytes, &timer, |mut source| {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let identities = (
            std::ptr::from_mut(&mut source),
            std::ptr::from_mut(&mut work),
            std::ptr::from_mut(&mut budget),
            std::ptr::from_mut(&mut scratch),
        );
        let mut arena = [0; 4096];
        let mut body = [0; 7];
        let mut parts = [Part::default(); 8];
        let mut headers = [None; 8];
        let mut retained = structure(
            &mut source,
            bytes.len(),
            blob_structure::Buffers {
                header_bytes: &mut arena,
                body: &mut body,
                parts: &mut parts,
                headers: &mut headers,
            },
            &mut work,
            &mut budget,
        );
        assert_eq!(retained.parts.len(), 3);
        let mut scratch = &mut scratch;
        let mut storages = [Storage::new(), Storage::new(), Storage::new()];
        for (index, storage) in storages.iter_mut().enumerate() {
            let part = retained.parts[index];
            assert_eq!(
                retained.headers[index].unwrap().source_end,
                crate::header_select::SourceEnd::Prefix
            );
            let mut resident_storage = Storage::new();
            let mut resident_work = meter();
            let mut resident_budget = HeaderBudget::new();
            let mut resident_scratch = Scratch::new();
            let mut resident = label_json::Cursor::new(
                mime_part_headers::Entity {
                    source: &bytes[part.entity_start as usize..part.entity_end as usize],
                    base: part.entity_start,
                    source_end: crate::header_select::SourceEnd::Eof,
                    header_limit: 1024,
                    context: part.context(),
                },
                resident_storage.backing(),
                &mut resident_work,
                &mut resident_budget,
                &mut resident_scratch,
            )
            .unwrap();
            for _ in 0..100_000 {
                if resident.poll(Tick(1)).unwrap() == Status::Complete {
                    break;
                }
            }
            let expected = resident.finish(Tick(1)).unwrap().0;
            let mut cursor = Cursor::new(
                retained,
                (index + 1) as u16,
                storage.backing(),
                scratch,
                Tick(1),
            )
            .unwrap();
            drain(&mut cursor).unwrap();
            let result = cursor.finish(Tick(1)).unwrap();
            assert_eq!(result.metadata.metadata.headers, expected.headers);
            assert_eq!(
                result.metadata.metadata.labels.content_id,
                expected.labels.content_id
            );
            assert_eq!(
                result.metadata.metadata.labels.content_language,
                expected.labels.content_language
            );
            assert_eq!(
                result.metadata.metadata.location.value,
                expected.location.value
            );
            if index == 1 {
                assert_eq!(
                    result.metadata.metadata.headers.content_type,
                    b"message/rfc822"
                );
            }
            if index == 2 {
                assert_eq!(
                    result.metadata.metadata.headers.filename,
                    Some("café.html".as_bytes())
                );
                assert_eq!(result.metadata.node.class.media, Media::Html);
                assert_eq!(
                    result.metadata.node.class.disposition,
                    Disposition::Attachment
                );
            }
            retained = result.structure;
            scratch = result.scratch;
        }
        assert_eq!(
            (
                std::ptr::from_mut(retained.source),
                std::ptr::from_mut(retained.work),
                std::ptr::from_mut(retained.budget),
                std::ptr::from_mut(scratch)
            ),
            identities
        );
    });
}
#[test]
fn native_cached_and_finish_expiry_hides_complete_metadata() {
    for mode in 0..4 {
        let timer = Timer::new();
        crate::store_fs::with_pinned_fixture(b"\nbody", &timer, |mut source| {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut storage = Storage::new();
            let mut arena = [0; 2048];
            let mut body = [0; 1];
            let mut parts = [Part::default(); 8];
            let mut headers = [None; 8];
            let retained = structure(
                &mut source,
                5,
                blob_structure::Buffers {
                    header_bytes: &mut arena,
                    body: &mut body,
                    parts: &mut parts,
                    headers: &mut headers,
                },
                &mut work,
                &mut budget,
            );
            let mut cursor =
                Cursor::new(retained, 1, storage.backing(), &mut scratch, Tick(1)).unwrap();
            drain(&mut cursor).unwrap();
            assert!(cursor.value().is_some());
            assert_eq!(cursor.poll(Tick(1)), Ok(Status::Complete));
            assert!(cursor.value().is_some());
            let before = timer.calls.load(Ordering::Relaxed);
            let expiry = if mode % 2 == 0 { 1 } else { 5 };
            timer.after(expiry);
            if mode < 2 {
                let error = Error::Policy(PolicyError::Deadline);
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert!(matches!(cursor.finish(Tick(1)),Err(found) if found == error));
            } else {
                assert!(matches!(
                    cursor.finish(Tick(1)),
                    Err(Error::Policy(PolicyError::Deadline))
                ));
            }
            assert_eq!(timer.calls.load(Ordering::Relaxed) - before, expiry);
        });
    }
}
#[test]
fn incomplete_finish_and_invalid_ordinal_refuse() {
    for ordinal in [0, 1, 2] {
        let timer = Timer::new();
        crate::store_fs::with_pinned_fixture(b"\nbody", &timer, |mut source| {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut storage = Storage::new();
            let mut arena = [0; 2048];
            let mut body = [0; 1];
            let mut parts = [Part::default(); 8];
            let mut headers = [None; 8];
            let retained = structure(
                &mut source,
                5,
                blob_structure::Buffers {
                    header_bytes: &mut arena,
                    body: &mut body,
                    parts: &mut parts,
                    headers: &mut headers,
                },
                &mut work,
                &mut budget,
            );
            let cursor = Cursor::new(retained, ordinal, storage.backing(), &mut scratch, Tick(1));
            if ordinal == 1 {
                assert!(matches!(
                    cursor.unwrap().finish(Tick(1)),
                    Err(Error::Interpretation(bound::Error::Metadata(
                        label_json::Error::InvalidState
                    )))
                ));
            } else {
                assert!(matches!(
                    cursor,
                    Err(Error::Interpretation(bound::Error::PartOrdinal))
                ));
            }
        });
    }
}
fn with_structure(
    bytes: &[u8],
    limit: usize,
    test: impl FnOnce(blob_structure::Retained<'_, '_, '_, '_, '_, '_>, &Timer),
) {
    let timer = Timer::new();
    crate::store_fs::with_pinned_fixture(bytes, &timer, |mut source| {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut arena = [0; 4096];
        let mut body = [0; 1];
        let mut parts = [Part::default(); 8];
        let mut headers = [None; 8];
        let mut options = settings(bytes.len());
        options.limits.header_bytes = limit;
        let mut cursor = blob_structure::Cursor::new(
            &mut source,
            options,
            blob_structure::Buffers {
                header_bytes: &mut arena,
                body: &mut body,
                parts: &mut parts,
                headers: &mut headers,
            },
            &mut work,
            &mut budget,
            Tick(1),
        )
        .unwrap();
        for _ in 0..100_000 {
            if cursor.poll(Tick(1)).unwrap() == mime_structure::Status::Complete {
                break;
            }
        }
        test(cursor.finish(Tick(1)).unwrap(), &timer);
    });
}
#[test]
fn native_metadata_parity_includes_eof_empty_and_exact_header_limits() {
    for (bytes, limit) in [
        (b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n--x--\r\n".as_slice(),1024),
        (b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\n--x--\r\n",1024),
        (b"Content-Type: multipart/mixed;boundary=x\n\n--x\nX: a\n folded\n\nbody\n--x--\n",1024),
        (b"X: y\n\nbody",5), (b"\nbody",1), (b"X: y",4), (b"",1),
    ] {
        with_structure(bytes,limit,|retained,_| {
            let mut storage: [Storage;8]=std::array::from_fn(|_|Storage::new());
            let mut scratch=Scratch::new();let mut scratch=&mut scratch;
            let mut retained = retained;
            for (index,storage) in storage.iter_mut().take(retained.parts.len()).enumerate() {
                let part=retained.parts[index];
                let mut backing=Storage::new();let mut work=meter();let mut budget=HeaderBudget::new();let mut resident_scratch=Scratch::new();
                let mut resident=label_json::Cursor::new(mime_part_headers::Entity {source:&bytes[part.entity_start as usize..part.entity_end as usize],base:part.entity_start,source_end:crate::header_select::SourceEnd::Eof,header_limit:limit as u64,context:part.context()},backing.backing(),&mut work,&mut budget,&mut resident_scratch).unwrap();
                for _ in 0..100_000 {if resident.poll(Tick(1)).unwrap()==Status::Complete{break;}}
                let (expected,work,budget,_)=resident.finish(Tick(1)).unwrap();
                let expected_node=bound::classify(part,expected.headers,Tick(1),work,budget).unwrap();
                let mut cursor=Cursor::new(retained,(index+1) as u16,storage.backing(),scratch,Tick(1)).unwrap();
                drain(&mut cursor).unwrap();
                assert_eq!(cursor.poll(Tick(1)),Ok(Status::Complete));
                assert!(cursor.value().is_some());
                let result=cursor.finish(Tick(1)).unwrap();
                assert_eq!(result.metadata.metadata.headers,expected.headers);
                assert_eq!(result.metadata.metadata.labels.content_id,expected.labels.content_id);
                assert_eq!(result.metadata.metadata.labels.content_language,expected.labels.content_language);
                assert_eq!(result.metadata.metadata.location.value,expected.location.value);
                assert_eq!(result.metadata.node,expected_node);
                assert_eq!(result.metadata.node.parent,part.parent);
                assert_eq!(result.metadata.node.depth,part.depth);
                retained=result.structure;scratch=result.scratch;
            }
        });
    }
}
#[test]
fn fabricated_correspondence_and_completed_body_disagreement_refuse() {
    for kind in 0..6 {
        with_structure(b"\nbody", 1, |retained, _| {
            let mut parts = [retained.parts[0]];
            let mut headers = [retained.headers[0]];
            match kind {
                0 => headers[0] = None,
                1 => parts[0].ordinal = 2,
                2 => parts[0].entity_start += 1,
                3 => parts[0].body_start += 1,
                _ => (),
            }
            let retained = blob_structure::Retained {
                parts: &parts,
                headers: &headers,
                ..retained
            };
            let mut storage = Storage::new();
            let mut scratch = Scratch::new();
            let cursor = Cursor::new(retained, 1, storage.backing(), &mut scratch, Tick(1));
            let error = Error::Interpretation(bound::Error::InvalidState);
            if kind < 4 {
                assert!(matches!(cursor,Err(found) if found == error));
            } else {
                let mut cursor = cursor.unwrap();
                drain(&mut cursor).unwrap();
                cursor.part.body_start += 1;
                assert!(cursor.value().is_none());
                if kind == 4 {
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                }
                assert!(matches!(cursor.finish(Tick(1)),Err(found) if found == error));
            }
        });
    }
}
#[test]
fn construction_and_active_checks_cover_native_and_job_expiry() {
    for (ordinal, expiry) in [(1, 1), (1, 3), (1, 5), (2, 3)] {
        with_structure(b"\nbody", 1, |retained, timer| {
            let mut storage = Storage::new();
            let mut scratch = Scratch::new();
            let before = timer.calls.load(Ordering::Relaxed);
            timer.after(expiry);
            let result = Cursor::new(retained, ordinal, storage.backing(), &mut scratch, Tick(1));
            if expiry == 5 {
                assert!(result.is_ok());
            } else {
                assert!(matches!(result, Err(Error::Policy(PolicyError::Deadline))));
            }
            assert_eq!(timer.calls.load(Ordering::Relaxed) - before, expiry.min(4));
        });
    }
    with_structure(b"\nbody", 1, |retained, _| {
        let mut storage = Storage::new();
        let mut scratch = Scratch::new();
        assert!(matches!(
            Cursor::new(retained, 1, storage.backing(), &mut scratch, Tick(100)),
            Err(Error::Interpretation(bound::Error::Admission(
                crate::nfc::Error::Work(crate::admission::work::Stop::Deadline)
            )))
        ));
    });
    for explicit in [false, true] {
        with_structure(
            b"Content-Type: text/plain\n\nbody",
            1024,
            |retained, timer| {
                let mut storage = Storage::new();
                let mut scratch = Scratch::new();
                let mut cursor =
                    Cursor::new(retained, 1, storage.backing(), &mut scratch, Tick(1)).unwrap();
                assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
                let before = timer.calls.load(Ordering::Relaxed);
                let expiry = if explicit { 3 } else { 5 };
                timer.after(expiry);
                let error = Error::Policy(PolicyError::Deadline);
                if explicit {
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                } else {
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                }
                assert_eq!(timer.calls.load(Ordering::Relaxed) - before, expiry);
                assert!(cursor.value().is_none());
                assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert!(matches!(cursor.finish(Tick(1)),Err(found) if found == error));
            },
        );
    }
}
#[test]
fn incomplete_prefix_and_short_replay_ceiling_retire_metadata() {
    for (bytes, prefix) in [
        (b"X: y".as_slice(), true),
        (b"X: y\n\nbody".as_slice(), false),
    ] {
        with_structure(bytes, 1024, |retained, _| {
            let mut headers = [retained.headers[0]];
            if prefix {
                headers[0].as_mut().unwrap().source_end = crate::header_select::SourceEnd::Prefix;
            } else {
                headers[0].as_mut().unwrap().end.header_bytes -= 1;
            }
            let retained = blob_structure::Retained {
                headers: &headers,
                ..retained
            };
            let mut storage = Storage::new();
            let mut scratch = Scratch::new();
            let mut cursor =
                Cursor::new(retained, 1, storage.backing(), &mut scratch, Tick(1)).unwrap();
            let inner = if prefix {
                td_mime::metadata::Error::Truncated
            } else {
                td_mime::metadata::Error::Headers(td_mime::headers::Error::HeaderLimit)
            };
            let error = Error::Interpretation(bound::Error::Metadata(label_json::Error::Headers(
                mime_part_headers::Error::Metadata(inner),
            )));
            assert_eq!(drain(&mut cursor), Err(error));
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(Tick(1)), Err(error));
            assert!(matches!(cursor.finish(Tick(1)),Err(found) if found == error));
        });
    }
}
