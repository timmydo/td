//! Retain one supplied frame's changes until its complete checksum passes.
use crate::{
    format::{
        container::Error, frame_stream, operation::Value, Error as FormatError, ObjectType,
        Sequence,
    },
    ports::{Change, ChangeAction, ChangeCursor, ChangeRecord, Crypto},
};

/// Private scratch contents; allocate slots before entering the read path.
#[derive(Clone, Copy)]
pub struct Cell {
    change: Change,
    ordinal: u32,
}
impl Cell {
    pub const EMPTY: Self = Self {
        change: Change {
            kind: ObjectType::Mailbox,
            id: [0; 16],
            action: ChangeAction::Created,
        },
        ordinal: 0,
    };
}

/// No retained CHANGE is exposed before consuming completion succeeds.
pub struct Collector<'c, 's, C: Crypto> {
    verifier: frame_stream::Verifier<'c, C>,
    cells: &'s mut [Cell],
    count: usize,
    failed: Option<Error>,
}
impl<'c, 's, C: Crypto> Collector<'c, 's, C> {
    pub fn new(
        crypto: &'c C,
        previous: Sequence,
        header: &[u8],
        cells: &'s mut [Cell],
    ) -> Result<Self, Error> {
        Ok(Self {
            verifier: frame_stream::Verifier::new(crypto, previous, header)?,
            cells,
            count: 0,
            failed: None,
        })
    }
    /// Checked frame header only; payload and checksum completion remain pending.
    pub const fn header(&self) -> crate::format::frame_header::Header {
        self.verifier.header()
    }
    /// One exact operation; row bodies are validated and then discarded.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.push_operation(bytes);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn push_operation(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let entry = self.verifier.push(bytes)?;
        if let Value::Change(change) = entry.operation.value() {
            let ordinal = u32::try_from(entry.ordinal).map_err(|_| FormatError::Overflow)?;
            let count = self.count.checked_add(1).ok_or(FormatError::Overflow)?;
            let cell = self
                .cells
                .get_mut(self.count)
                .ok_or(FormatError::OutputFull)?;
            *cell = Cell { change, ordinal };
            self.count = count;
        }
        Ok(())
    }
    /// Local frame completion only; file selection, actual pins and final references remain external.
    pub fn finish(self, footer: &[u8]) -> Result<CompleteChanges<'s>, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let summary = self.verifier.finish(footer)?;
        if self.count > self.cells.len() {
            return Err(FormatError::InvalidValue.into());
        }
        Ok(CompleteChanges {
            summary,
            cells: self.cells,
            count: self.count,
        })
    }
}

/// Copied changes in stored operation order; owns no file or runtime view pin.
pub struct CompleteChanges<'s> {
    summary: frame_stream::Summary,
    cells: &'s mut [Cell],
    count: usize,
}
impl<'s> CompleteChanges<'s> {
    pub const fn summary(&self) -> frame_stream::Summary {
        self.summary
    }
    pub fn len(&self) -> usize {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    /// Consume the checked result and recover the original full scratch capacity.
    pub fn into_cells(self) -> &'s mut [Cell] {
        self.cells
    }
    pub fn records(&self) -> impl ExactSizeIterator<Item = ChangeRecord> + '_ {
        let sequence = self.summary.header().sequence;
        self.cells
            .iter()
            .take(self.count)
            .map(move |cell| ChangeRecord {
                cursor: ChangeCursor {
                    sequence,
                    operation: cell.ordinal,
                },
                change: cell.change,
            })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::format::{frame, operation::Operation, Table, MAX_FRAME_OPERATIONS};
    use td_crypto::Provider;

    fn frame(operations: &[Operation<'_>]) -> Vec<u8> {
        let payload: usize = operations.iter().map(|op| op.encoded_len().unwrap()).sum();
        let mut bytes = vec![0; 104 + payload];
        let mut offset = 64;
        for operation in operations {
            offset += operation.encode(&mut bytes[offset..]).unwrap();
        }
        frame::seal(
            &Provider,
            Sequence::from_u64(7),
            operations.len(),
            &mut bytes,
        )
        .unwrap();
        bytes
    }
    fn collect<'s>(bytes: &[u8], cells: &'s mut [Cell]) -> Result<CompleteChanges<'s>, Error> {
        let mut collector = Collector::new(&Provider, Sequence::from_u64(6), &bytes[..64], cells)?;
        let mut offset = 64;
        while offset < bytes.len() - 40 {
            let n = crate::format::operation::extent(&bytes[offset..offset + 12])?;
            collector.push(&bytes[offset..offset + n])?;
            offset += n;
        }
        collector.finish(&bytes[offset..])
    }
    #[test]
    fn mixed_operations_preserve_changes_ordinals_and_copied_ownership() {
        let id = [8; 16];
        let mut operations = vec![Operation::put(Table::Threads, &id, &[]).unwrap()];
        for kind in [
            ObjectType::Mailbox,
            ObjectType::Email,
            ObjectType::Thread,
            ObjectType::EmailSubmission,
        ] {
            for action in [
                ChangeAction::Created,
                ChangeAction::Updated,
                ChangeAction::Destroyed,
            ] {
                operations.push(Operation::delete(Table::Blobs, &id).unwrap());
                operations.push(Operation::change(kind, action, &id));
            }
        }
        let mut bytes = frame(&operations);
        let expected: Vec<_> = frame::Frame::decode(&Provider, Sequence::from_u64(6), &bytes)
            .unwrap()
            .operations()
            .filter_map(|entry| {
                let entry = entry.unwrap();
                if let Value::Change(change) = entry.operation.value() {
                    Some(ChangeRecord {
                        cursor: ChangeCursor {
                            sequence: Sequence::from_u64(7),
                            operation: entry.ordinal as u32,
                        },
                        change,
                    })
                } else {
                    None
                }
            })
            .collect();
        let mut cells = [Cell::EMPTY; 12];
        let complete = collect(&bytes, &mut cells).unwrap();
        bytes.fill(0);
        assert_eq!(complete.len(), 12);
        assert!(!complete.is_empty());
        assert_eq!(complete.records().len(), 12);
        assert_eq!(complete.records().collect::<Vec<_>>(), expected);
        assert_eq!(complete.summary().header().operations, 25);
        assert!(std::mem::size_of::<Collector<'_, '_, Provider>>() <= 1024);
    }
    #[test]
    fn maximum_changes_and_empty_rows_obey_independent_slot_capacity() {
        let operation = Operation::change(ObjectType::Email, ChangeAction::Updated, &[9; 16]);
        let bytes = frame(&vec![operation; MAX_FRAME_OPERATIONS]);
        let mut cells = vec![Cell::EMPTY; MAX_FRAME_OPERATIONS];
        let complete = collect(&bytes, &mut cells).unwrap();
        assert_eq!(complete.len(), MAX_FRAME_OPERATIONS);
        for (ordinal, record) in complete.records().enumerate() {
            assert_eq!(record.cursor.operation, ordinal as u32);
            assert_eq!(record.change.id, [9; 16]);
        }
        let decoded = frame::Frame::decode(&Provider, Sequence::from_u64(6), &bytes).unwrap();
        for (record, entry) in complete.records().zip(decoded.operations()) {
            let entry = entry.unwrap();
            assert_eq!(record.cursor.sequence, decoded.header().sequence);
            assert_eq!(record.cursor.operation as usize, entry.ordinal);
            assert_eq!(Value::Change(record.change), entry.operation.value());
        }
        assert!(matches!(
            collect(&bytes, &mut cells[..MAX_FRAME_OPERATIONS - 1]),
            Err(Error::Format(FormatError::OutputFull))
        ));
        let only_rows = frame(&[Operation::delete(Table::Threads, &[3; 16]).unwrap()]);
        let empty = collect(&only_rows, &mut []).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.records().next(), None);
        assert_eq!(empty.summary().header().operations, 1);
    }
    #[test]
    fn malformed_input_capacity_and_footer_errors_cannot_expose_partial_changes() {
        let operation = Operation::change(ObjectType::Mailbox, ChangeAction::Created, &[2; 16]);
        let bytes = frame(&[operation, operation]);
        for malformed in [false, true] {
            let mut cells = [Cell::EMPTY; 1];
            let mut collector =
                Collector::new(&Provider, Sequence::from_u64(6), &bytes[..64], &mut cells).unwrap();
            collector.push(&bytes[64..92]).unwrap();
            let bad = if malformed {
                &bytes[92..119]
            } else {
                &bytes[92..120]
            };
            let error = collector.push(bad).unwrap_err();
            assert_eq!(
                error,
                Error::Format(if malformed {
                    FormatError::Truncated
                } else {
                    FormatError::OutputFull
                })
            );
            assert_eq!(collector.push(&bytes[92..120]), Err(error));
            assert!(matches!(collector.finish(&bytes[120..]), Err(e) if e == error));
        }
        let mut cells = [Cell::EMPTY; 2];
        let mut bad = bytes.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(matches!(collect(&bad, &mut cells), Err(Error::Checksum)));
        let mut collector =
            Collector::new(&Provider, Sequence::from_u64(6), &bytes[..64], &mut cells).unwrap();
        collector.push(&bytes[64..92]).unwrap();
        assert!(matches!(
            collector.finish(&bytes[120..]),
            Err(Error::Format(FormatError::Truncated))
        ));
        let replacement = frame(&[Operation::change(
            ObjectType::EmailSubmission,
            ChangeAction::Destroyed,
            &[5; 16],
        )]);
        let reused = collect(&replacement, &mut cells).unwrap();
        assert_eq!(
            reused.records().collect::<Vec<_>>(),
            vec![ChangeRecord {
                cursor: ChangeCursor {
                    sequence: Sequence::from_u64(7),
                    operation: 0
                },
                change: Change {
                    kind: ObjectType::EmailSubmission,
                    id: [5; 16],
                    action: ChangeAction::Destroyed
                },
            }]
        );
        assert!(
            Collector::new(&Provider, Sequence::from_u64(7), &bytes[..64], &mut cells).is_err()
        );
    }
    #[test]
    fn draining_recovers_full_scratch_for_shorter_and_empty_frames() {
        let mut cells = [Cell::EMPTY; 4];
        let original = cells.as_ptr();
        let change = Operation::change(ObjectType::Email, ChangeAction::Created, &[6; 16]);
        let first = frame(&[change, change]);
        let complete = collect(&first, &mut cells).unwrap();
        assert_eq!(complete.len(), 2);
        let cells = complete.into_cells();
        assert_eq!(cells.len(), 4);
        assert_eq!(cells.as_ptr(), original);
        let next = frame(&[Operation::change(
            ObjectType::Thread,
            ChangeAction::Destroyed,
            &[7; 16],
        )]);
        let complete = collect(&next, cells).unwrap();
        let records: Vec<_> = complete.records().collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].change.kind, ObjectType::Thread);
        assert_eq!(records[0].change.id, [7; 16]);
        let empty = frame(&[Operation::delete(Table::Threads, &[1; 16]).unwrap()]);
        let complete = collect(&empty, complete.into_cells()).unwrap();
        assert!(complete.is_empty());
        assert_eq!(complete.records().next(), None);
        let cells = complete.into_cells();
        assert_eq!(cells.len(), 4);
        assert_eq!(cells.as_ptr(), original);
    }
}
