//! Bounded policy over supplied checked frames; file selection and real view pins are external.
use crate::{
    format::{frame_stream::Summary, ObjectType, Sequence},
    frame_changes::CompleteChanges,
    ports::{ChangeCursor, ChangeStep, Error, ViewIdentity},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    /// The driver must locate this exact frame under its own bounded I/O budget.
    NeedFrame {
        sequence: Sequence,
    },
    Change(ChangeStep),
}
#[derive(Clone, Copy)]
struct Progress {
    summary: Summary,
    next: usize,
}
/// One forward scan for one object kind. Returned steps are provisional until
/// the driver establishes selected-file/graph validity and owns the real pins.
pub struct Cursor {
    view: ViewIdentity,
    kind: ObjectType,
    after: ChangeCursor,
    frame: Option<Progress>,
    failed: Option<Error>,
    #[cfg(test)]
    examined: usize,
}
impl Cursor {
    pub fn new(view: ViewIdentity, after: ChangeCursor, kind: ObjectType) -> Result<Self, Error> {
        if kind == ObjectType::Identity
            || view.history_floor > view.committed_sequence
            || after.sequence > view.committed_sequence
        {
            return Err(Error::Invalid);
        }
        if after.sequence < view.history_floor
            || (after.sequence == view.history_floor && after.operation != u32::MAX)
        {
            return Err(Error::HistoryLost);
        }
        Ok(Self {
            view,
            kind,
            after,
            frame: None,
            failed: None,
            #[cfg(test)]
            examined: 0,
        })
    }
    pub const fn after(&self) -> ChangeCursor {
        self.after
    }
    pub const fn is_failed(&self) -> bool {
        self.failed.is_some()
    }
    /// A repeated caller cursor must be exactly the last returned record/boundary.
    /// No I/O is performed here; the driver checks deadlines and real ownership.
    pub fn poll(
        &mut self,
        view: ViewIdentity,
        after: ChangeCursor,
        frame: Option<&CompleteChanges<'_>>,
    ) -> Result<Step, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.advance(view, after, frame);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn advance(
        &mut self,
        view: ViewIdentity,
        after: ChangeCursor,
        frame: Option<&CompleteChanges<'_>>,
    ) -> Result<Step, Error> {
        if view != self.view {
            return Err(Error::Conflict);
        }
        if after != self.after {
            return Err(Error::Invalid);
        }
        if after.sequence == view.committed_sequence && after.operation == u32::MAX {
            return Ok(Step::Change(ChangeStep::Complete));
        }
        let needed = if after.operation == u32::MAX {
            after.sequence.successor().map_err(|_| Error::Invalid)?
        } else {
            after.sequence
        };
        let Some(frame) = frame else {
            return Ok(Step::NeedFrame { sequence: needed });
        };
        let summary = frame.summary();
        if summary.header().sequence != needed {
            return Err(Error::Corrupt);
        }
        let mut progress = match self.frame {
            Some(progress) if progress.summary == summary => progress,
            Some(_) => return Err(Error::Corrupt),
            None => {
                if after.sequence == needed
                    && usize::try_from(after.operation).map_err(|_| Error::Invalid)?
                        >= summary.header().operations
                {
                    return Err(Error::Invalid);
                }
                Progress { summary, next: 0 }
            }
        };
        while progress.next < frame.len() {
            #[cfg(test)]
            {
                self.examined += 1;
            }
            let record = frame.record(progress.next).ok_or(Error::Corrupt)?;
            progress.next = progress.next.checked_add(1).ok_or(Error::Corrupt)?;
            if (record.cursor.sequence != after.sequence
                || record.cursor.operation > after.operation)
                && record.change.kind == self.kind
            {
                self.frame = Some(progress);
                self.after = record.cursor;
                return Ok(Step::Change(ChangeStep::Record(record)));
            }
        }
        self.frame = None;
        self.after = ChangeCursor {
            sequence: needed,
            operation: u32::MAX,
        };
        Ok(Step::Change(ChangeStep::Advanced { through: needed }))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::{
        format::{
            frame,
            operation::{Operation, Value},
            ObjectType, Table,
        },
        frame_changes::{Cell, Collector},
        ids::{AccountId, StoreEpoch},
        ports::ChangeAction,
    };
    use td_crypto::Provider;
    fn view(end: u64, floor: u64) -> ViewIdentity {
        ViewIdentity {
            account: AccountId::from_bytes([1; 16]),
            epoch: StoreEpoch::from_bytes([2; 16]),
            generation: 1,
            checkpoint: Sequence::from_u64(floor),
            segment: 1,
            committed_offset: 96,
            committed_sequence: Sequence::from_u64(end),
            history_floor: Sequence::from_u64(floor),
        }
    }
    fn at(sequence: u64, operation: u32) -> ChangeCursor {
        ChangeCursor {
            sequence: Sequence::from_u64(sequence),
            operation,
        }
    }
    fn boundary(sequence: u64) -> ChangeCursor {
        at(sequence, u32::MAX)
    }
    fn frame_bytes(sequence: u64, ops: &[Operation<'_>]) -> Vec<u8> {
        let length = ops
            .iter()
            .map(|op| op.encoded_len().unwrap())
            .sum::<usize>();
        let mut bytes = vec![0; 104 + length];
        let mut offset = 64;
        for op in ops {
            offset += op.encode(&mut bytes[offset..]).unwrap();
        }
        frame::seal(
            &Provider,
            Sequence::from_u64(sequence),
            ops.len(),
            &mut bytes,
        )
        .unwrap();
        bytes
    }
    fn collect<'s>(bytes: &[u8], cells: &'s mut [Cell]) -> CompleteChanges<'s> {
        let header = crate::format::frame_header::Header::decode(&Provider, &bytes[..64]).unwrap();
        let prior = Sequence::from_u64(header.sequence.number() - 1);
        let decoded = frame::Frame::decode(&Provider, prior, bytes).unwrap();
        let mut collector = Collector::new(&Provider, prior, &bytes[..64], cells).unwrap();
        let mut offset = 64;
        for entry in decoded.operations() {
            let length = entry.unwrap().operation.encoded_len().unwrap();
            collector.push(&bytes[offset..offset + length]).unwrap();
            offset += length;
        }
        collector.finish(&bytes[offset..]).unwrap()
    }
    #[test]
    fn filtering_preserves_ordinals_actions_and_empty_frame_boundaries() {
        let view = view(5, 3);
        let first = frame_bytes(
            4,
            &[
                Operation::delete(Table::Threads, &[1; 16]).unwrap(),
                Operation::change(ObjectType::Email, ChangeAction::Created, &[2; 16]),
                Operation::change(ObjectType::Thread, ChangeAction::Updated, &[3; 16]),
                Operation::change(ObjectType::Email, ChangeAction::Destroyed, &[2; 16]),
            ],
        );
        let second = frame_bytes(
            5,
            &[Operation::change(
                ObjectType::Thread,
                ChangeAction::Updated,
                &[3; 16],
            )],
        );
        let mut cells = [Cell::EMPTY; 3];
        let first = collect(&first, &mut cells);
        let mut cursor = Cursor::new(view, boundary(3), ObjectType::Email).unwrap();
        assert_eq!(
            cursor.poll(view, cursor.after(), None),
            Ok(Step::NeedFrame {
                sequence: Sequence::from_u64(4)
            })
        );
        for (ordinal, action) in [(1, ChangeAction::Created), (3, ChangeAction::Destroyed)] {
            let Step::Change(ChangeStep::Record(record)) =
                cursor.poll(view, cursor.after(), Some(&first)).unwrap()
            else {
                panic!("record")
            };
            assert_eq!(record.cursor, at(4, ordinal));
            assert_eq!(record.change.action, action);
            assert_eq!(record.change.id, [2; 16]);
        }
        assert_eq!(
            cursor.poll(view, cursor.after(), Some(&first)),
            Ok(Step::Change(ChangeStep::Advanced {
                through: Sequence::from_u64(4)
            }))
        );
        assert_eq!(cursor.after(), boundary(4));
        assert_eq!(
            cursor.poll(view, cursor.after(), None),
            Ok(Step::NeedFrame {
                sequence: Sequence::from_u64(5)
            })
        );
        let second = collect(&second, first.into_cells());
        assert_eq!(
            cursor.poll(view, cursor.after(), Some(&second)),
            Ok(Step::Change(ChangeStep::Advanced {
                through: Sequence::from_u64(5)
            }))
        );
        for _ in 0..2 {
            assert_eq!(
                cursor.poll(view, cursor.after(), None),
                Ok(Step::Change(ChangeStep::Complete))
            );
        }
        assert!(std::mem::size_of::<Cursor>() <= 512);
    }
    #[test]
    fn maximum_frame_resumes_once_and_matches_whole_decoder() {
        let ops: Vec<_> = (0..4096)
            .map(|index| {
                Operation::change(
                    match index % 4 {
                        0 => ObjectType::Email,
                        1 => ObjectType::Thread,
                        2 => ObjectType::Mailbox,
                        _ => ObjectType::EmailSubmission,
                    },
                    ChangeAction::Updated,
                    &[7; 16],
                )
            })
            .collect();
        let bytes = frame_bytes(1, &ops);
        let mut cells = vec![Cell::EMPTY; 4096];
        let complete = collect(&bytes, &mut cells);
        for kind in [
            ObjectType::Email,
            ObjectType::Thread,
            ObjectType::Mailbox,
            ObjectType::EmailSubmission,
        ] {
            for ordinal in [None, Some(0), Some(2047), Some(4095)] {
                let initial = ordinal.map_or(boundary(0), |n| at(1, n));
                let mut cursor = Cursor::new(view(1, 0), initial, kind).unwrap();
                let expected: Vec<_> = frame::Frame::decode(&Provider, Sequence::default(), &bytes)
                    .unwrap()
                    .operations()
                    .map(Result::unwrap)
                    .filter_map(|entry| match entry.operation.value() {
                        Value::Change(change)
                            if change.kind == kind
                                && ordinal.is_none_or(|n| entry.ordinal as u32 > n) =>
                        {
                            Some((entry.ordinal as u32, change))
                        }
                        _ => None,
                    })
                    .collect();
                let mut observed = Vec::new();
                let mut examined = 0;
                loop {
                    match cursor
                        .poll(view(1, 0), cursor.after(), Some(&complete))
                        .unwrap()
                    {
                        Step::Change(ChangeStep::Record(record)) => {
                            observed.push((record.cursor.operation, record.change));
                            let progress = cursor.frame.unwrap();
                            assert!(progress.next > examined);
                            examined = progress.next;
                        }
                        Step::Change(ChangeStep::Advanced { through }) => {
                            assert_eq!(through, Sequence::from_u64(1));
                            break;
                        }
                        other => panic!("{other:?}"),
                    }
                }
                assert_eq!(observed, expected);
                assert_eq!(cursor.examined, complete.len());
                assert_eq!(
                    cursor.poll(view(1, 0), cursor.after(), None),
                    Ok(Step::Change(ChangeStep::Complete))
                );
            }
        }
    }
    #[test]
    fn saved_progress_survives_missing_frame_and_equivalent_result() {
        let bytes = frame_bytes(
            1,
            &[
                Operation::change(ObjectType::Email, ChangeAction::Created, &[1; 16]),
                Operation::change(ObjectType::Email, ChangeAction::Updated, &[2; 16]),
                Operation::change(ObjectType::Email, ChangeAction::Destroyed, &[3; 16]),
            ],
        );
        let mut a = [Cell::EMPTY; 3];
        let mut b = [Cell::EMPTY; 3];
        let first = collect(&bytes, &mut a);
        let equivalent = collect(&bytes, &mut b);
        let v = view(1, 0);
        let mut cursor = Cursor::new(v, boundary(0), ObjectType::Email).unwrap();
        for (ordinal, frame) in [first, equivalent].iter().enumerate() {
            assert_eq!(
                cursor.poll(v, cursor.after(), None),
                Ok(Step::NeedFrame {
                    sequence: Sequence::from_u64(1)
                })
            );
            assert_eq!(cursor.examined, ordinal);
            assert!(
                matches!(cursor.poll(v,cursor.after(),Some(frame)),Ok(Step::Change(ChangeStep::Record(record))) if record.cursor.operation as usize == ordinal)
            );
            assert_eq!(cursor.examined, ordinal + 1);
        }
    }
    #[test]
    fn row_only_final_sequence_advances_before_exhausted_completion() {
        let bytes = frame_bytes(
            u64::MAX,
            &[Operation::delete(Table::Threads, &[1; 16]).unwrap()],
        );
        let frame = collect(&bytes, &mut []);
        let v = view(u64::MAX, u64::MAX - 1);
        let mut cursor = Cursor::new(v, boundary(u64::MAX - 1), ObjectType::Email).unwrap();
        assert_eq!(
            cursor.poll(v, cursor.after(), None),
            Ok(Step::NeedFrame {
                sequence: Sequence::from_u64(u64::MAX)
            })
        );
        assert_eq!(
            cursor.poll(v, cursor.after(), Some(&frame)),
            Ok(Step::Change(ChangeStep::Advanced {
                through: Sequence::from_u64(u64::MAX)
            }))
        );
        assert_eq!(
            cursor.poll(v, cursor.after(), None),
            Ok(Step::Change(ChangeStep::Complete))
        );
    }
    #[test]
    fn invalid_ranges_changed_views_and_frame_substitution_fail_sticky() {
        for (view, after, kind, error) in [
            (
                view(5, 3),
                boundary(2),
                ObjectType::Email,
                Error::HistoryLost,
            ),
            (view(5, 3), at(3, 0), ObjectType::Email, Error::HistoryLost),
            (view(5, 3), boundary(6), ObjectType::Email, Error::Invalid),
            (view(5, 6), boundary(5), ObjectType::Email, Error::Invalid),
            (
                view(5, 3),
                boundary(3),
                ObjectType::Identity,
                Error::Invalid,
            ),
        ] {
            assert!(matches!(Cursor::new(view,after,kind),Err(actual) if actual==error));
        }
        for end in [0, u64::MAX] {
            let v = view(end, end);
            let mut cursor = Cursor::new(v, boundary(end), ObjectType::Email).unwrap();
            assert_eq!(
                cursor.poll(v, boundary(end), None),
                Ok(Step::Change(ChangeStep::Complete))
            );
        }
        let bytes = frame_bytes(
            1,
            &[
                Operation::change(ObjectType::Email, ChangeAction::Created, &[1; 16]),
                Operation::change(ObjectType::Email, ChangeAction::Updated, &[2; 16]),
            ],
        );
        let replacement = frame_bytes(
            1,
            &[Operation::change(
                ObjectType::Email,
                ChangeAction::Created,
                &[3; 16],
            )],
        );
        let wrong = frame_bytes(
            2,
            &[Operation::change(
                ObjectType::Email,
                ChangeAction::Created,
                &[1; 16],
            )],
        );
        let mut a = [Cell::EMPTY; 2];
        let mut b = [Cell::EMPTY; 1];
        let mut c = [Cell::EMPTY; 1];
        let frame = collect(&bytes, &mut a);
        let replacement = collect(&replacement, &mut b);
        let wrong = collect(&wrong, &mut c);
        for mode in 0..4 {
            let v = view(2, 0);
            let mut cursor = Cursor::new(
                v,
                if mode == 2 { at(1, 2) } else { boundary(0) },
                ObjectType::Email,
            )
            .unwrap();
            let result = match mode {
                0 => {
                    let mut changed = v;
                    changed.history_floor = Sequence::from_u64(1);
                    cursor.poll(changed, cursor.after(), None)
                }
                1 => cursor.poll(v, boundary(1), Some(&frame)),
                2 => cursor.poll(v, cursor.after(), Some(&frame)),
                _ => cursor.poll(v, cursor.after(), Some(&wrong)),
            };
            let expected = match mode {
                0 => Error::Conflict,
                1 | 2 => Error::Invalid,
                _ => Error::Corrupt,
            };
            assert_eq!(result, Err(expected));
            assert!(cursor.is_failed());
            assert_eq!(cursor.poll(v, cursor.after(), Some(&frame)), Err(expected));
        }
        let v = view(1, 0);
        let mut cursor = Cursor::new(v, boundary(0), ObjectType::Email).unwrap();
        assert!(matches!(
            cursor.poll(v, cursor.after(), Some(&frame)),
            Ok(Step::Change(ChangeStep::Record(_)))
        ));
        assert_eq!(
            cursor.poll(v, cursor.after(), Some(&replacement)),
            Err(Error::Corrupt)
        );
        assert_eq!(
            cursor.poll(v, cursor.after(), Some(&frame)),
            Err(Error::Corrupt)
        );
    }
}
