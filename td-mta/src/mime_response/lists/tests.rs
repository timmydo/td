#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use crate::mime_response::lists::*;
use crate::{
    admission::work::{Charge, Stop},
    nfc::Scratch,
};
use {
    crate::mime_response::ordered::tests::complete, crate::mime_response::ordered::tests::meter,
    crate::mime_response::ordered::tests::structure, crate::mime_response::ordered::tests::Storage,
    crate::mime_response::ordered::tests::SOURCE, crate::mime_response::ordered::Classifying,
};
pub(in crate::mime_response) const ALTERNATIVE: &[u8] = concat!(
    "Content-Type: multipart/alternative;boundary=a\r\n\r\n--a\r\n",
    "Content-Type: multipart/mixed;boundary=m\r\n\r\n--m\r\n",
    "Content-Type: text/plain\r\n\r\nx\r\n--m\r\n",
    "Content-Type: image/png\r\n\r\nx\r\n--m--\r\n--a--\r\n"
)
.as_bytes();
pub(in crate::mime_response) struct Lists {
    text: [u16; 8],
    html: [u16; 8],
    attachments: [u16; 8],
    flags: [u8; 8],
}
impl Lists {
    pub(in crate::mime_response) fn new() -> Self {
        Self {
            text: [42; 8],
            html: [42; 8],
            attachments: [42; 8],
            flags: [42; 8],
        }
    }
    pub(in crate::mime_response) fn backing(
        &mut self,
        caps: [usize; 4],
    ) -> mime_body_lists::Backing<'_> {
        mime_body_lists::Backing {
            text: &mut self.text[..caps[0]],
            html: &mut self.html[..caps[1]],
            attachments: &mut self.attachments[..caps[2]],
            membership: &mut self.flags[..caps[3]],
        }
    }
}
pub(in crate::mime_response) fn classify<'a, 'w, 'n>(
    source: &'a [u8],
    base: u64,
    parts: &'w mut [Part],
    nodes: &'n mut [Node],
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
) -> Classified<'a, 'w, 'n> {
    let binding = structure(source, base, parts, work, budget);
    let mut ordered = Classifying::new(binding, nodes).unwrap();
    let mut storage = Storage::new();
    let mut scratch = Scratch::new();
    let count = ordered.total().unwrap();
    complete(&mut ordered, &mut storage, &mut scratch, count);
    ordered.finish(Tick(1)).unwrap()
}
fn drain(cursor: &mut Selecting<'_, '_, '_>) -> Result<usize, Error> {
    for turn in 1..100000 {
        if cursor.poll(Tick(1))? == Status::Complete {
            return Ok(turn);
        }
    }
    panic!("selection did not complete")
}
pub(in crate::mime_response) fn costs(work: &Meter, budget: &HeaderBudget) -> [u64; 5] {
    [
        HeaderBudget::new().source_bytes_remaining() - budget.source_bytes_remaining(),
        HeaderBudget::new().steps_remaining() - budget.steps_remaining(),
        100_000_000 - work.remaining().io_bytes,
        10_000_000 - work.remaining().records,
        10_000_000 - work.remaining().output_bytes,
    ]
}
#[test]
fn lists_match_direct_costs_original_source_and_owner_identity() {
    for source in [SOURCE, ALTERNATIVE] {
        for base in [0, 17, u64::MAX - source.len() as u64] {
            let mut parts = [Part::default(); 8];
            let mut nodes = [Node::default(); 8];
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut lists = Lists::new();
            let classified = classify(source, base, &mut parts, &mut nodes, &mut work, &mut budget);
            let (view, work, budget) = classified.finish(Tick(1)).unwrap();
            let descriptors = view.parts.to_vec();
            let original_nodes = view.nodes.to_vec();
            let before = costs(work, budget);
            let mut direct = mime_body_lists::Cursor::new(
                view.nodes,
                &Limits::default(),
                lists.backing([8; 4]),
                work,
                budget,
            )
            .unwrap();
            let mut turns = 0;
            loop {
                turns += 1;
                if direct.poll(Tick(1)).unwrap() == Status::Complete {
                    break;
                }
            }
            let (view, work, budget) = direct.finish(Tick(1)).unwrap();
            let expected = (
                view.text.to_vec(),
                view.html.to_vec(),
                view.attachments.to_vec(),
                view.has_attachment,
            );
            let total = costs(work, budget);
            assert_eq!(&before[..2], &total[..2]);
            if source == SOURCE {
                assert_eq!(expected, (vec![3], vec![3], vec![2], true))
            } else {
                assert_eq!(expected, (vec![3, 4], vec![3, 4], vec![], false))
            }
            let mut parts = [Part::default(); 8];
            let forged = Node {
                parent: 42,
                depth: 63,
                ..Node::default()
            };
            let mut nodes = [forged; 8];
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut lists = Lists::new();
            let pointers = (
                std::ptr::from_mut(&mut work),
                std::ptr::from_mut(&mut budget),
            );
            let classified = classify(source, base, &mut parts, &mut nodes, &mut work, &mut budget);
            let header_limit = classified.structure.header_limit;
            let mut selecting = Selecting::new(
                classified,
                &Limits::default(),
                lists.backing([8; 4]),
                Tick(1),
            )
            .unwrap();
            assert!(selecting.value().is_none());
            assert!(std::ptr::eq(selecting.binding.source, source));
            assert_eq!(selecting.binding.base, base);
            assert_eq!(selecting.binding.header_limit, header_limit);
            selecting.check_deadline(Tick(1)).unwrap();
            assert_eq!(drain(&mut selecting).unwrap(), turns);
            assert_eq!(selecting.poll(Tick(100)), Ok(Status::Complete));
            let mut selected = selecting.finish(Tick(1)).unwrap();
            selected.check_deadline(Tick(1)).unwrap();
            assert!(std::ptr::eq(selected.binding.source, source));
            assert_eq!(selected.binding.header_limit, header_limit);
            let (view, work, budget) = selected.finish(Tick(1)).unwrap();
            assert_eq!(view.structure.parts, descriptors);
            assert_eq!(view.structure.nodes, original_nodes);
            assert_eq!(
                (
                    view.lists.text.to_vec(),
                    view.lists.html.to_vec(),
                    view.lists.attachments.to_vec(),
                    view.lists.has_attachment
                ),
                expected
            );
            assert_eq!(costs(work, budget), total);
            assert_eq!(
                (std::ptr::from_mut(work), std::ptr::from_mut(budget)),
                pointers
            );
            assert_eq!(
                &nodes[original_nodes.len()..],
                &[forged; 8][original_nodes.len()..]
            );
            assert_eq!(
                &lists.flags[original_nodes.len()..],
                &[42; 8][original_nodes.len()..]
            );
            assert_eq!(
                &lists.text[expected.0.len()..],
                &[42; 8][expected.0.len()..]
            );
            assert_eq!(
                &lists.html[expected.1.len()..],
                &[42; 8][expected.1.len()..]
            );
            assert_eq!(
                &lists.attachments[expected.2.len()..],
                &[42; 8][expected.2.len()..]
            );
        }
    }
}
#[test]
fn every_list_prefix_and_complete_owner_freshly_refuses_late_admission() {
    for source in [SOURCE, ALTERNATIVE] {
        let mut parts = [Part::default(); 8];
        let mut nodes = [Node::default(); 8];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut lists = Lists::new();
        let classified = classify(source, 17, &mut parts, &mut nodes, &mut work, &mut budget);
        let mut cursor = Selecting::new(
            classified,
            &Limits::default(),
            lists.backing([8; 4]),
            Tick(1),
        )
        .unwrap();
        let turns = drain(&mut cursor).unwrap();
        for prefix in 0..=turns {
            for explicit in [false, true] {
                let mut parts = [Part::default(); 8];
                let mut nodes = [Node::default(); 8];
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                let mut lists = Lists::new();
                let classified =
                    classify(source, 17, &mut parts, &mut nodes, &mut work, &mut budget);
                let mut cursor = Selecting::new(
                    classified,
                    &Limits::default(),
                    lists.backing([8; 4]),
                    Tick(1),
                )
                .unwrap();
                for _ in 0..prefix {
                    cursor.poll(Tick(1)).unwrap();
                }
                assert_eq!(cursor.value().is_some(), prefix == turns);
                let error = Error::BodyLists(mime_body_lists::Error::Work(Stop::Deadline));
                if explicit {
                    assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                }
                assert_eq!(
                    cursor
                        .finish(if explicit { Tick(1) } else { Tick(100) })
                        .err(),
                    Some(error)
                );
                assert_eq!(work.stopped(), Some(Stop::Deadline));
            }
        }
        for explicit in [false, true] {
            let mut parts = [Part::default(); 8];
            let mut nodes = [Node::default(); 8];
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut lists = Lists::new();
            let classified = classify(source, 0, &mut parts, &mut nodes, &mut work, &mut budget);
            let mut cursor = Selecting::new(
                classified,
                &Limits::default(),
                lists.backing([8; 4]),
                Tick(1),
            )
            .unwrap();
            drain(&mut cursor).unwrap();
            let mut owner = cursor.finish(Tick(1)).unwrap();
            assert!(owner.value().is_some());
            let error = Error::Admission(crate::nfc::Error::Work(Stop::Deadline));
            if explicit {
                assert_eq!(owner.check_deadline(Tick(100)), Err(error));
                assert!(owner.value().is_none());
            }
            assert_eq!(
                owner
                    .finish(if explicit { Tick(1) } else { Tick(100) })
                    .err(),
                Some(error)
            );
        }
    }
}
#[test]
fn constructor_and_premature_finish_admit_before_local_errors() {
    for (limits, error) in [
        (
            Limits {
                mime_depth: 0,
                ..Limits::default()
            },
            mime_body_lists::Error::InvalidLimits,
        ),
        (
            Limits {
                mime_parts: 2,
                mime_depth: 1,
                ..Limits::default()
            },
            mime_body_lists::Error::PartLimit,
        ),
    ] {
        let mut parts = [Part::default(); 8];
        let mut nodes = [Node::default(); 8];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut lists = Lists::new();
        let classified = classify(SOURCE, 0, &mut parts, &mut nodes, &mut work, &mut budget);
        assert_eq!(
            Selecting::new(classified, &limits, lists.backing([8; 4]), Tick(1)).err(),
            Some(Error::BodyLists(error))
        );
        assert_eq!(work.stopped(), None);
    }
    for mode in 0..5 {
        let mut parts = [Part::default(); 8];
        let mut nodes = [Node::default(); 8];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut lists = Lists::new();
        let mut classified = classify(SOURCE, 0, &mut parts, &mut nodes, &mut work, &mut budget);
        let error = if mode == 2 {
            let bytes = classified.structure.budget.source_bytes_remaining();
            assert!(classified
                .structure
                .budget
                .charge(
                    classified.structure.work,
                    Tick(1),
                    bytes + 1,
                    0,
                    &mut crate::nfc::Credit::new()
                )
                .is_err());
            Error::Admission(crate::nfc::Error::InterpretationLimit)
        } else {
            Error::Admission(crate::nfc::Error::Work(Stop::Deadline))
        };
        if mode == 1 {
            assert_eq!(classified.check_deadline(Tick(100)), Err(error));
        }
        if mode < 3 {
            let invalid = Limits {
                mime_depth: 0,
                ..Limits::default()
            };
            assert_eq!(
                Selecting::new(
                    classified,
                    &invalid,
                    lists.backing([0; 4]),
                    if mode == 0 { Tick(100) } else { Tick(1) }
                )
                .err(),
                Some(error)
            );
        } else {
            let cursor = Selecting::new(
                classified,
                &Limits::default(),
                lists.backing([8; 4]),
                Tick(1),
            )
            .unwrap();
            assert_eq!(
                cursor
                    .finish(if mode == 3 { Tick(1) } else { Tick(100) })
                    .err(),
                Some(if mode == 3 {
                    Error::BodyLists(mime_body_lists::Error::InvalidState)
                } else {
                    Error::BodyLists(mime_body_lists::Error::Work(Stop::Deadline))
                })
            );
        }
    }
}
#[test]
fn original_job_cuts_and_all_output_capacities_retire_selection() {
    let mut parts = [Part::default(); 8];
    let mut nodes = [Node::default(); 8];
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut lists = Lists::new();
    let classified = classify(SOURCE, 0, &mut parts, &mut nodes, &mut work, &mut budget);
    let mut cursor = Selecting::new(
        classified,
        &Limits::default(),
        lists.backing([8; 4]),
        Tick(1),
    )
    .unwrap();
    drain(&mut cursor).unwrap();
    let (_, work, budget) = cursor.finish(Tick(1)).unwrap().finish(Tick(1)).unwrap();
    let total = costs(work, budget);
    for mode in 0..7 {
        let mut parts = [Part::default(); 8];
        let mut nodes = [Node::default(); 8];
        let mut work = meter();
        let stop = match mode {
            0 => {
                work.charge(
                    Tick(1),
                    Charge {
                        io_bytes: 100_000_000 - (total[2] - 1),
                        ..Charge::default()
                    },
                )
                .unwrap();
                Some(Stop::IoBytes)
            }
            1 => {
                work.charge(
                    Tick(1),
                    Charge {
                        records: 10_000_000 - (total[3] - 1),
                        ..Charge::default()
                    },
                )
                .unwrap();
                Some(Stop::Records)
            }
            2 => {
                work.charge(
                    Tick(1),
                    Charge {
                        output_bytes: 10_000_000 - (total[4] - 1),
                        ..Charge::default()
                    },
                )
                .unwrap();
                Some(Stop::OutputBytes)
            }
            _ => None,
        };
        let mut budget = HeaderBudget::new();
        let mut lists = Lists::new();
        let classified = classify(SOURCE, 0, &mut parts, &mut nodes, &mut work, &mut budget);
        let mut caps = [8; 4];
        if mode >= 3 {
            caps[mode - 3] = 0;
        }
        let mut cursor =
            Selecting::new(classified, &Limits::default(), lists.backing(caps), Tick(1)).unwrap();
        let error = Error::BodyLists(
            stop.map(mime_body_lists::Error::Work)
                .unwrap_or(mime_body_lists::Error::OutputCapacity),
        );
        assert_eq!(drain(&mut cursor), Err(error));
        assert!(cursor.value().is_none());
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        assert_eq!(work.stopped(), stop);
    }
}
