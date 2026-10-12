#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    blob_part,
    mime_part_headers::{self, label_json},
    nfc::Scratch,
    ports::{Clock, Deadline, Error as PolicyError, Time},
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

fn limits() -> Limits {
    Limits {
        header_bytes: 1024,
        mime_depth: 8,
        mime_parts: 8,
    }
}
fn exercise(
    bytes: &[u8],
    test: impl FnOnce(blob_structure::Retained<'_, '_, '_, '_, '_, '_>, &[Node], &Timer),
) {
    let timer = Timer::new();
    crate::store_fs::with_pinned_fixture(bytes, &timer, |mut source| {
        let mut storage: [Storage; 8] = std::array::from_fn(|_| Storage::new());
        let mut scratch = Scratch::new();
        let mut scratch = &mut scratch;
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut arena = [0; 8192];
        let mut body = [0; 7];
        let mut parts = [Part::default(); 8];
        let mut headers = [None; 8];
        let mut nodes = [Node::default(); 8];
        let mut cursor = blob_structure::Cursor::new(
            &mut source,
            blob_structure::Settings {
                extent: 0..bytes.len() as u64,
                limits: limits(),
            },
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
            if cursor.poll(Tick(1)).unwrap() == crate::mime_structure::Status::Complete {
                break;
            }
        }
        let mut retained = cursor.finish(Tick(1)).unwrap();
        let count = retained.parts.len();
        for (index, storage) in storage.iter_mut().take(count).enumerate() {
            let mut cursor = blob_part::Cursor::new(
                retained,
                (index + 1) as u16,
                storage.backing(),
                scratch,
                Tick(1),
            )
            .unwrap();
            for _ in 0..100_000 {
                if cursor.poll(Tick(1)).unwrap() == mime_part_headers::Status::Complete {
                    break;
                }
            }
            let result = cursor.finish(Tick(1)).unwrap();
            nodes[index] = result.metadata.node;
            retained = result.structure;
            scratch = result.scratch;
        }
        test(retained, &nodes[..count], &timer);
    });
}
struct Lists {
    text: [u16; 8],
    html: [u16; 8],
    attachments: [u16; 8],
    membership: [u8; 8],
}
impl Lists {
    fn new() -> Self {
        Self {
            text: [0; 8],
            html: [0; 8],
            attachments: [0; 8],
            membership: [0; 8],
        }
    }
    fn backing(&mut self) -> Backing<'_> {
        Backing {
            text: &mut self.text,
            html: &mut self.html,
            attachments: &mut self.attachments,
            membership: &mut self.membership,
        }
    }
}
fn drain(cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_>) -> Result<(), Error> {
    for _ in 0..100_000 {
        if cursor.poll(Tick(1))? == Status::Complete {
            return Ok(());
        }
    }
    panic!("native body lists did not finish");
}
fn counters(work: &Meter, budget: &HeaderBudget) -> [u64; 5] {
    let left = work.remaining();
    [
        budget.source_bytes_remaining(),
        budget.steps_remaining(),
        left.io_bytes,
        left.records,
        left.output_bytes,
    ]
}
fn own(view: View<'_>) -> (Vec<u16>, Vec<u16>, Vec<u16>, bool) {
    (
        view.text.to_vec(),
        view.html.to_vec(),
        view.attachments.to_vec(),
        view.has_attachment,
    )
}
const MIXED: &[u8] = concat!(
    "Content-Type: multipart/mixed;boundary=x\r\n\r\n",
    "--x\r\nContent-Type: multipart/alternative;boundary=y\r\n\r\n",
    "--y\r\n\r\nplain\r\n",
    "--y\r\nContent-Type: text/html\r\n\r\n<b>html</b>\r\n--y--\r\n",
    "--x\r\nContent-Type: application/pdf\r\n",
    "Content-Disposition: attachment;filename=a.pdf\r\n\r\npdf\r\n--x--\r\n"
)
.as_bytes();
#[test]
fn native_lists_match_shared_selection_costs_and_preserve_custody() {
    for bytes in [b"\nplain".as_slice(), MIXED] {
        exercise(bytes, |retained, nodes, _| {
            let original = (
                std::ptr::from_mut(retained.source),
                std::ptr::from_mut(retained.work),
                std::ptr::from_mut(retained.budget),
                retained.parts.as_ptr(),
                retained.headers.as_ptr(),
                retained.remaining.as_ptr(),
                retained.header_bytes,
            );
            let before = counters(retained.work, retained.budget);
            let mut output = Lists::new();
            let mut cursor =
                Cursor::new(retained, nodes, limits(), output.backing(), Tick(1)).unwrap();
            assert!(cursor.value().is_none());
            for _ in nodes {
                assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
                assert!(cursor.value().is_none());
            }
            drain(&mut cursor).unwrap();
            assert_eq!(cursor.poll(Tick(1)), Ok(Status::Complete));
            let result = cursor.finish(Tick(1)).unwrap();
            let got = own(result.lists);
            let after = counters(result.structure.work, result.structure.budget);
            assert_eq!(
                (
                    std::ptr::from_mut(result.structure.source),
                    std::ptr::from_mut(result.structure.work),
                    std::ptr::from_mut(result.structure.budget),
                    result.structure.parts.as_ptr(),
                    result.structure.headers.as_ptr(),
                    result.structure.remaining.as_ptr(),
                    result.structure.header_bytes
                ),
                original
            );
            assert_eq!(result.nodes.as_ptr(), nodes.as_ptr());
            if bytes == MIXED {
                assert_eq!(got, (vec![3], vec![4], vec![5], true));
            } else {
                assert_eq!(got, (vec![1], vec![1], vec![], false));
            }
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut output = Lists::new();
            let before_shared = counters(&work, &budget);
            let mut child = mime_body_lists::Cursor::new(
                nodes,
                limits(),
                output.backing(),
                &mut work,
                &mut budget,
            )
            .unwrap();
            for _ in 0..100_000 {
                if child.poll(Tick(1)).unwrap() == Status::Complete {
                    break;
                }
            }
            let (view, work, budget) = child.finish(Tick(1)).unwrap();
            assert_eq!(got, own(view));
            let after_shared = counters(work, budget);
            let mut expected =
                std::array::from_fn::<_, 5, _>(|i| before_shared[i] - after_shared[i]);
            expected[3] += nodes.len() as u64;
            assert_eq!(
                std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                expected
            );
        });
    }
}
#[test]
fn mismatched_correspondence_capacity_and_unfinished_refuse() {
    for kind in 0..5 {
        exercise(b"\nplain", |retained, nodes, _| {
            let mut altered = [nodes[0]];
            if kind == 1 {
                altered[0].parent = 1;
            }
            if kind == 2 {
                altered[0].depth += 1;
            }
            let mut output = Lists::new();
            let mut backing = output.backing();
            if kind == 3 {
                backing.text = &mut [];
            }
            let result = Cursor::new(
                retained,
                if kind == 0 { &[] } else { &altered },
                limits(),
                backing,
                Tick(1),
            );
            if kind == 0 {
                assert!(matches!(
                    result,
                    Err(Error::Interpretation(bound::Error::InvalidState))
                ));
                return;
            }
            let mut cursor = result.unwrap();
            if kind == 4 {
                assert!(matches!(
                    cursor.finish(Tick(1)),
                    Err(Error::Interpretation(bound::Error::InvalidState))
                ));
                return;
            }
            let expected = Error::Interpretation(if kind == 3 {
                bound::Error::BodyLists(mime_body_lists::Error::OutputCapacity)
            } else {
                bound::Error::InvalidState
            });
            assert_eq!(drain(&mut cursor), Err(expected));
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(Tick(1)), Err(expected));
            assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == expected));
        });
    }
}
#[test]
fn native_constructor_active_cached_and_finish_checks_are_fresh() {
    for mode in 0..6 {
        exercise(b"\nplain", |retained, nodes, timer| {
            let mut output = Lists::new();
            if mode == 0 {
                timer.after(3);
                assert!(matches!(
                    Cursor::new(retained, nodes, limits(), output.backing(), Tick(1)),
                    Err(Error::Policy(PolicyError::Deadline))
                ));
                return;
            }
            let mut cursor =
                Cursor::new(retained, nodes, limits(), output.backing(), Tick(1)).unwrap();
            if (2..=4).contains(&mode) {
                drain(&mut cursor).unwrap();
                assert!(cursor.value().is_some());
            }
            if mode == 5 {
                for _ in 0..=nodes.len() {
                    assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
                }
                assert!(cursor.child.is_some());
            }
            let before = timer.calls.load(Ordering::Relaxed);
            // Each successful native check samples twice; expire at the final fence.
            timer.after(if mode == 4 { 3 } else { 5 });
            let error = Error::Policy(PolicyError::Deadline);
            if mode == 3 {
                assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
            } else {
                if mode == 4 {
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                } else {
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                }
                if mode == 1 {
                    assert_eq!(cursor.binding.as_ref().unwrap().checked, 1);
                }
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
            }
            assert_eq!(
                timer.calls.load(Ordering::Relaxed) - before,
                if mode == 4 { 3 } else { 5 }
            );
        });
    }
}
#[test]
fn original_job_refusals_and_shared_limit_validation_remain_terminal() {
    for selecting in [false, true] {
        exercise(b"\nplain", |retained, nodes, _| {
            let mut output = Lists::new();
            let mut cursor =
                Cursor::new(retained, nodes, limits(), output.backing(), Tick(1)).unwrap();
            if selecting {
                for _ in 0..=nodes.len() {
                    assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
                }
            }
            let error = Error::Interpretation(if selecting {
                bound::Error::BodyLists(mime_body_lists::Error::Work(
                    crate::admission::work::Stop::Deadline,
                ))
            } else {
                bound::Error::Admission(crate::nfc::Error::Work(
                    crate::admission::work::Stop::Deadline,
                ))
            });
            assert_eq!(cursor.poll(Tick(100)), Err(error));
            assert!(cursor.value().is_none());
            assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
            assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
        });
    }
    exercise(b"\nplain", |retained, nodes, _| {
        let mut output = Lists::new();
        let mut options = limits();
        options.mime_depth = 0;
        let mut cursor = Cursor::new(retained, nodes, options, output.backing(), Tick(1)).unwrap();
        let error = Error::Interpretation(bound::Error::BodyLists(
            mime_body_lists::Error::InvalidLimits,
        ));
        assert_eq!(drain(&mut cursor), Err(error));
        assert!(cursor.value().is_none());
        assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
    });
    exercise(b"\nplain", |retained, nodes, _| {
        retained
            .work
            .charge(
                Tick(1),
                Charge {
                    records: retained.work.remaining().records,
                    ..Charge::default()
                },
            )
            .unwrap();
        let mut output = Lists::new();
        let mut cursor = Cursor::new(retained, nodes, limits(), output.backing(), Tick(1)).unwrap();
        let error = Error::Interpretation(bound::Error::Admission(crate::nfc::Error::Work(
            crate::admission::work::Stop::Records,
        )));
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert!(cursor.value().is_none());
        assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
    });
    exercise(b"\nplain", |retained, nodes, timer| {
        let mut output = Lists::new();
        timer.after(3);
        assert!(matches!(
            Cursor::new(retained, nodes, limits(), output.backing(), Tick(100)),
            Err(Error::Policy(PolicyError::Deadline))
        ));
    });
}

#[test]
fn multipart_counts_and_later_coordinates_refuse() {
    for kind in 0..6 {
        exercise(MIXED, |retained, nodes, timer| {
            let mut altered = nodes.to_vec();
            let mut parts = retained.parts.to_vec();
            match kind {
                0 => {
                    altered.pop();
                }
                1 => altered.push(Node::default()),
                2 => altered[3].parent = 0,
                3 => altered[3].depth = 0,
                4 => {
                    parts[3].ordinal = 1;
                }
                _ => altered[3].parent = 0,
            }
            let retained = blob_structure::Retained {
                parts: &parts,
                ..retained
            };
            let mut output = Lists::new();
            let result = Cursor::new(retained, &altered, limits(), output.backing(), Tick(1));
            if kind < 2 {
                assert!(matches!(
                    result,
                    Err(Error::Interpretation(bound::Error::InvalidState))
                ));
                return;
            }
            let mut cursor = result.unwrap();
            for _ in 0..3 {
                assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
            }
            assert_eq!(cursor.binding.as_ref().unwrap().checked, 3);
            if kind == 5 {
                timer.after(5);
            }
            let expected = if kind == 5 {
                Error::Policy(PolicyError::Deadline)
            } else {
                Error::Interpretation(bound::Error::InvalidState)
            };
            assert_eq!(cursor.poll(Tick(1)), Err(expected));
            assert_eq!(cursor.binding.as_ref().unwrap().checked, 3);
            assert!(cursor.value().is_none());
            assert_eq!(cursor.check_deadline(Tick(1)), Err(expected));
        });
    }
    exercise(MIXED, |retained, nodes, timer| {
        let mut output = Lists::new();
        let mut cursor = Cursor::new(retained, nodes, limits(), output.backing(), Tick(1)).unwrap();
        for _ in 0..3 {
            cursor.poll(Tick(1)).unwrap();
        }
        timer.after(5);
        assert_eq!(
            cursor.poll(Tick(1)),
            Err(Error::Policy(PolicyError::Deadline))
        );
        assert_eq!(cursor.binding.as_ref().unwrap().checked, 4);
    });
}

#[test]
fn completed_job_checks_and_incomplete_child_finish_refuse() {
    for mode in 0..5 {
        exercise(MIXED, |retained, nodes, _| {
            let mut output = Lists::new();
            if mode == 0 {
                assert!(matches!(
                    Cursor::new(retained, nodes, limits(), output.backing(), Tick(100)),
                    Err(Error::Interpretation(bound::Error::Admission(
                        crate::nfc::Error::Work(crate::admission::work::Stop::Deadline)
                    )))
                ));
                return;
            }
            let mut cursor =
                Cursor::new(retained, nodes, limits(), output.backing(), Tick(1)).unwrap();
            if mode == 4 {
                for _ in 0..=nodes.len() {
                    assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
                }
                assert!(cursor.child.is_some());
                assert!(cursor.value().is_none());
                assert!(matches!(
                    cursor.finish(Tick(1)),
                    Err(Error::Interpretation(bound::Error::BodyLists(
                        mime_body_lists::Error::InvalidState
                    )))
                ));
                return;
            }
            drain(&mut cursor).unwrap();
            let expected = Error::Interpretation(bound::Error::BodyLists(
                mime_body_lists::Error::Work(crate::admission::work::Stop::Deadline),
            ));
            if mode == 3 {
                assert!(matches!(cursor.finish(Tick(100)), Err(found) if found == expected));
            } else {
                let result = if mode == 1 {
                    cursor.poll(Tick(100)).map(|_| ())
                } else {
                    cursor.check_deadline(Tick(100))
                };
                assert_eq!(result, Err(expected));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1)), Err(expected));
            }
        });
    }
}
