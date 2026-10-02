//! Bounded replay of supplied active-journal bytes, separate from I/O and view pins.
use crate::{
    format::{
        self,
        frame::DecodeError,
        frame_header::Header,
        journal_stream::{Error, Summary, Verifier},
        key::Key,
        row, Sequence, Table, FRAME_FOOTER_BYTES, FRAME_HEADER_BYTES, MAX_JOURNAL_FRAME_BYTES,
        MAX_JOURNAL_OPERATIONS, MAX_KEY_BYTES, OPERATION_HEADER_BYTES,
    },
    ports::{Crypto, Mutation, OperationKind},
};

#[derive(Clone, Copy, Debug)]
struct Descriptor {
    sequence: Sequence,
    key_offset: u32,
    value_offset: u32,
    value_len: u32,
    key_len: u16,
    tag: u16,
    ordinal: u16,
    kind: OperationKind,
}
/// Caller-owned fixed slots; no references or heap allocation per operation.
#[derive(Clone, Copy, Debug)]
pub struct Cell {
    entry: Option<Descriptor>,
}
impl Cell {
    pub const EMPTY: Self = Self { entry: None };
}
impl Descriptor {
    fn key(self, bytes: &[u8]) -> Option<&[u8]> {
        let start = usize::try_from(self.key_offset).ok()?;
        let end = start.checked_add(usize::from(self.key_len))?;
        bytes.get(start..end)
    }
    fn prefix(self, bytes: &[u8]) -> Option<(bool, u16, &[u8])> {
        Some((
            matches!(self.kind, OperationKind::Change(_)),
            self.tag,
            self.key(bytes)?,
        ))
    }
    fn order(self, bytes: &[u8]) -> Option<(bool, u16, &[u8], Sequence, u16)> {
        let (change, tag, key) = self.prefix(bytes)?;
        Some((change, tag, key, self.sequence, self.ordinal))
    }
    fn row(self, bytes: &[u8]) -> Result<Entry<'_>, format::Error> {
        let key = self.key(bytes).ok_or(format::Error::InvalidValue)?;
        let table = Table::from_tag(self.tag)?;
        let mutation = match self.kind {
            OperationKind::Put => {
                let start =
                    usize::try_from(self.value_offset).map_err(|_| format::Error::Overflow)?;
                let end = start
                    .checked_add(
                        usize::try_from(self.value_len).map_err(|_| format::Error::Overflow)?,
                    )
                    .ok_or(format::Error::Overflow)?;
                let (key, row) = row::decode_record(
                    table,
                    key,
                    bytes.get(start..end).ok_or(format::Error::InvalidValue)?,
                )?;
                Mutation::Put { key, row }
            }
            OperationKind::Delete => {
                let key = Key::decode(table, key)?;
                key.validate_local()?;
                Mutation::Delete(key)
            }
            OperationKind::Change(_) => return Err(format::Error::InvalidValue),
        };
        Ok(Entry {
            mutation,
            sequence: self.sequence,
            ordinal: self.ordinal,
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Entry<'a> {
    pub mutation: Mutation<'a>,
    pub sequence: Sequence,
    pub ordinal: u16,
}
/// Checked supplied frames and latest-key lookup. Physical extent, selection,
/// complete final-view validity and actual pin ownership remain caller duties.
pub struct Overlay<'b, 's> {
    bytes: &'b [u8],
    cells: &'s [Cell],
    summary: Summary,
}
impl<'b, 's> Overlay<'b, 's> {
    /// Startup-allocated frames exclude the 96-byte journal header. Decode
    /// may overwrite cells, including on error. All stored operations consume slots.
    pub fn decode(
        crypto: &impl Crypto,
        header: &[u8],
        bytes: &'b [u8],
        cells: &'s mut [Cell],
    ) -> Result<Self, Error> {
        if bytes.len() > MAX_JOURNAL_FRAME_BYTES || cells.len() > MAX_JOURNAL_OPERATIONS {
            return Err(format::Error::Limit.into());
        }
        cells.fill(Cell::EMPTY);
        let mut verifier = Verifier::new(crypto, header)?;
        let mut offset = 0usize;
        let mut count = 0usize;
        while offset < bytes.len() {
            let prefix_end = offset
                .checked_add(FRAME_HEADER_BYTES)
                .ok_or(format::Error::Overflow)?;
            let remaining = bytes
                .len()
                .checked_sub(offset)
                .ok_or(format::Error::Overflow)?;
            let prefix = bytes
                .get(offset..prefix_end)
                .ok_or(DecodeError::Incomplete {
                    required: FRAME_HEADER_BYTES,
                    supplied: remaining,
                })?;
            let frame_header =
                Header::decode(crypto, prefix).map_err(|error| Error::Frame(error.into()))?;
            let end = offset
                .checked_add(frame_header.frame_bytes)
                .ok_or(format::Error::Overflow)?;
            let encoded = bytes.get(offset..end).ok_or(DecodeError::Incomplete {
                required: frame_header.frame_bytes,
                supplied: remaining,
            })?;
            let frame = verifier.push(encoded)?;
            let next_count = count
                .checked_add(frame.header().operations)
                .ok_or(format::Error::Overflow)?;
            if next_count > cells.len() {
                return Err(format::Error::OutputFull.into());
            }
            let mut cursor = prefix_end;
            for entry in frame.operations() {
                let entry = entry?;
                let op = entry.operation;
                let key_offset = cursor
                    .checked_add(OPERATION_HEADER_BYTES)
                    .ok_or(format::Error::Overflow)?;
                let value_offset = key_offset
                    .checked_add(op.key_bytes().len())
                    .ok_or(format::Error::Overflow)?;
                let next = value_offset
                    .checked_add(op.value_bytes().len())
                    .ok_or(format::Error::Overflow)?;
                let descriptor = Descriptor {
                    sequence: frame.header().sequence,
                    key_offset: u32::try_from(key_offset).map_err(|_| format::Error::Overflow)?,
                    value_offset: u32::try_from(value_offset)
                        .map_err(|_| format::Error::Overflow)?,
                    value_len: u32::try_from(op.value_bytes().len())
                        .map_err(|_| format::Error::Overflow)?,
                    key_len: u16::try_from(op.key_bytes().len())
                        .map_err(|_| format::Error::Overflow)?,
                    tag: op.type_tag(),
                    ordinal: u16::try_from(entry.ordinal).map_err(|_| format::Error::Overflow)?,
                    kind: op.kind(),
                };
                // Verify offsets against the decoded slices before sorting may use them.
                if descriptor.key(bytes) != Some(op.key_bytes())
                    || bytes.get(value_offset..next) != Some(op.value_bytes())
                {
                    return Err(format::Error::InvalidValue.into());
                }
                cells.get_mut(count).ok_or(format::Error::OutputFull)?.entry = Some(descriptor);
                count = count.checked_add(1).ok_or(format::Error::Overflow)?;
                cursor = next;
            }
            if count != next_count || cursor.checked_add(FRAME_FOOTER_BYTES) != Some(end) {
                return Err(format::Error::InvalidValue.into());
            }
            offset = end;
        }
        let summary = verifier.finish()?;
        let used = cells.get_mut(..count).ok_or(format::Error::OutputFull)?;
        // Every descriptor/range is checked above and remains immutable during sort.
        used.sort_unstable_by(|a, b| {
            a.entry
                .and_then(|v| v.order(bytes))
                .cmp(&b.entry.and_then(|v| v.order(bytes)))
        });
        Ok(Self {
            bytes,
            cells: used,
            summary,
        })
    }
    pub const fn summary(&self) -> Summary {
        self.summary
    }
    pub fn operation_count(&self) -> usize {
        self.cells.len()
    }
    /// None means absent from this overlay; a DELETE is returned as a tombstone.
    pub fn get(&self, key: Key<'_>) -> Result<Option<Entry<'b>>, format::Error> {
        key.validate_local()?;
        let mut encoded = [0; MAX_KEY_BYTES];
        let len = key.encode(&mut encoded)?;
        self.latest(
            key.table(),
            encoded.get(..len).ok_or(format::Error::InvalidValue)?,
        )
    }
    fn latest(&self, table: Table, key: &[u8]) -> Result<Option<Entry<'b>>, format::Error> {
        let target = (false, table.tag(), key);
        let end = self.cells.partition_point(|cell| {
            cell.entry
                .and_then(|v| v.prefix(self.bytes))
                .is_some_and(|v| v <= target)
        });
        let Some(index) = end.checked_sub(1) else {
            return Ok(None);
        };
        let descriptor = self
            .cells
            .get(index)
            .and_then(|v| v.entry)
            .ok_or(format::Error::InvalidValue)?;
        if descriptor.prefix(self.bytes) != Some(target) {
            return Ok(None);
        }
        Ok(Some(descriptor.row(self.bytes)?))
    }
    /// Ascending canonical key order, strictly after the supplied key. Includes
    /// tombstones so checkpoint merging can suppress deleted rows.
    pub fn next(
        &self,
        table: Table,
        after: Option<&[u8]>,
    ) -> Result<Option<Entry<'b>>, format::Error> {
        if let Some(key) = after {
            Key::decode(table, key)?.validate_local()?;
        }
        let target = (false, table.tag(), after.unwrap_or_default());
        let index = self.cells.partition_point(|cell| {
            cell.entry
                .and_then(|v| v.prefix(self.bytes))
                .is_some_and(|v| v <= target)
        });
        let Some(cell) = self.cells.get(index) else {
            return Ok(None);
        };
        let descriptor = cell.entry.ok_or(format::Error::InvalidValue)?;
        if matches!(descriptor.kind, OperationKind::Change(_)) || descriptor.tag != table.tag() {
            return Ok(None);
        }
        self.latest(
            table,
            descriptor
                .key(self.bytes)
                .ok_or(format::Error::InvalidValue)?,
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{
        format::{
            container::JournalHeader,
            frame,
            operation::Operation,
            row::{BlobKind, BlobRow, Row},
            ObjectType, JOURNAL_HEADER_BYTES,
        },
        ids::{AccountId, BlobId, MailboxId, StoreEpoch, ThreadId},
        ports::{ChangeAction, Digest},
    };
    use td_crypto::Provider;
    fn header(base: u64) -> [u8; JOURNAL_HEADER_BYTES] {
        let mut bytes = [0; JOURNAL_HEADER_BYTES];
        JournalHeader {
            account: AccountId::from_bytes([3; 16]),
            epoch: StoreEpoch::from_bytes([4; 16]),
            segment: 1,
            base: Sequence::from_u64(base),
        }
        .encode(&Provider, &mut bytes)
        .unwrap();
        bytes
    }
    fn frame(sequence: u64, operations: &[Operation<'_>]) -> Vec<u8> {
        let length = FRAME_HEADER_BYTES
            + FRAME_FOOTER_BYTES
            + operations
                .iter()
                .map(|op| op.encoded_len().unwrap())
                .sum::<usize>();
        let mut bytes = vec![0; length];
        let mut offset = FRAME_HEADER_BYTES;
        for op in operations {
            offset += op.encode(bytes.get_mut(offset..).unwrap()).unwrap();
        }
        frame::seal(
            &Provider,
            Sequence::from_u64(sequence),
            operations.len(),
            &mut bytes,
        )
        .unwrap();
        bytes
    }
    fn thread(id: u8) -> Key<'static> {
        Key::Thread(ThreadId::from_bytes([id; 16]))
    }
    #[test]
    fn latest_sequence_and_ordinal_preserve_tombstones_and_exclude_changes() {
        let first = frame(
            1,
            &[
                Operation::put(Table::Threads, &[1; 16], &[]).unwrap(),
                Operation::delete(Table::Threads, &[1; 16]).unwrap(),
                Operation::change(ObjectType::Thread, ChangeAction::Created, &[1; 16]),
                Operation::delete(Table::Threads, &[2; 16]).unwrap(),
            ],
        );
        let mut cells = [Cell::EMPTY; 8];
        let overlay = Overlay::decode(&Provider, &header(0), &first, &mut cells).unwrap();
        assert_eq!(
            overlay.get(thread(1)).unwrap(),
            Some(Entry {
                mutation: Mutation::Delete(thread(1)),
                sequence: Sequence::from_u64(1),
                ordinal: 1
            })
        );
        assert!(overlay
            .get(Key::Mailbox(MailboxId::from_bytes([1; 16])))
            .unwrap()
            .is_none());
        let second = frame(
            2,
            &[
                Operation::put(Table::Threads, &[2; 16], &[]).unwrap(),
                Operation::put(Table::Threads, &[1; 16], &[]).unwrap(),
                Operation::delete(Table::Threads, &[3; 16]).unwrap(),
            ],
        );
        let mut bytes = first;
        bytes.extend_from_slice(&second);
        let overlay = Overlay::decode(&Provider, &header(0), &bytes, &mut cells).unwrap();
        assert_eq!(overlay.operation_count(), 7);
        for (id, ordinal) in [(1, 1), (2, 0)] {
            assert_eq!(
                overlay.get(thread(id)).unwrap(),
                Some(Entry {
                    mutation: Mutation::Put {
                        key: thread(id),
                        row: Row::Thread
                    },
                    sequence: Sequence::from_u64(2),
                    ordinal
                })
            );
        }
        assert!(matches!(
            overlay.get(thread(3)).unwrap().unwrap().mutation,
            Mutation::Delete(_)
        ));
        assert!(overlay.get(thread(4)).unwrap().is_none());
        let first = overlay.next(Table::Threads, None).unwrap().unwrap();
        assert!(
            matches!(first.mutation,Mutation::Put {key:Key::Thread(id),..} if id==ThreadId::from_bytes([1;16]))
        );
        let second = overlay
            .next(Table::Threads, Some(&[1; 16]))
            .unwrap()
            .unwrap();
        assert!(
            matches!(second.mutation,Mutation::Put {key:Key::Thread(id),..} if id==ThreadId::from_bytes([2;16]))
        );
        assert!(
            matches!(overlay.next(Table::Threads,Some(&[2;16])).unwrap().unwrap().mutation,Mutation::Delete(Key::Thread(id)) if id==ThreadId::from_bytes([3;16]))
        );
        assert!(overlay
            .next(Table::Threads, Some(&[3; 16]))
            .unwrap()
            .is_none());
        assert!(overlay.next(Table::Mailboxes, None).unwrap().is_none());
        assert!(overlay.next(Table::Blobs, None).unwrap().is_none());
        assert!(overlay.next(Table::Threads, Some(&[0; 15])).is_err());
        let mut digest = Provider.sha256().unwrap();
        digest.update(&header(0)).unwrap();
        digest.update(&bytes).unwrap();
        assert_eq!(overlay.summary().digest(), digest.finish().unwrap());
        assert_eq!(overlay.summary().through(), Sequence::from_u64(2));
        assert_eq!(overlay.summary().frame_bytes(), bytes.len());
    }
    #[test]
    fn duplicate_blob_values_match_independent_last_write_model() {
        use std::collections::BTreeMap;
        let mut bytes = Vec::new();
        let mut expected = BTreeMap::new();
        for sequence in 1..=20 {
            let mut encoded_keys = Vec::new();
            let mut encoded_values = Vec::new();
            for ordinal in 0..40 {
                let id = ((sequence * 7 + ordinal * 11) % 23) as u8;
                encoded_keys.push([id; 16]);
                let value = BlobRow {
                    kind: BlobKind::Message,
                    length: sequence * 40 + ordinal,
                    digest: [id; 32],
                    created_at: ordinal as i64,
                };
                let mut encoded = [0; 49];
                assert_eq!(Row::Blob(value).encode(&mut encoded).unwrap(), 49);
                encoded_values.push(encoded);
                expected.insert(id, (sequence, ordinal as u16, value, ordinal % 3 == 0));
            }
            let operations: Vec<_> = encoded_keys
                .iter()
                .zip(&encoded_values)
                .enumerate()
                .map(|(ordinal, (key, value))| {
                    if ordinal % 3 == 0 {
                        Operation::delete(Table::Blobs, key).unwrap()
                    } else {
                        Operation::put(Table::Blobs, key, value).unwrap()
                    }
                })
                .collect();
            bytes.extend_from_slice(&frame(sequence, &operations));
        }
        let mut cells = vec![Cell::EMPTY; 800];
        let overlay = Overlay::decode(&Provider, &header(0), &bytes, &mut cells).unwrap();
        let mut cursor = None;
        for (id, (sequence, ordinal, row, deleted)) in expected {
            let entry = overlay
                .next(
                    Table::Blobs,
                    cursor.as_ref().map(|v: &[u8; 16]| v.as_slice()),
                )
                .unwrap()
                .unwrap();
            let key = Key::Blob(BlobId::from_bytes([id; 16]));
            let mutation = if deleted {
                Mutation::Delete(key)
            } else {
                Mutation::Put {
                    key,
                    row: Row::Blob(row),
                }
            };
            assert_eq!(
                entry,
                Entry {
                    mutation,
                    sequence: Sequence::from_u64(sequence),
                    ordinal
                }
            );
            assert_eq!(overlay.get(key).unwrap(), Some(entry));
            cursor = Some([id; 16]);
        }
        assert!(overlay
            .next(Table::Blobs, cursor.as_ref().map(|v| v.as_slice()))
            .unwrap()
            .is_none());
    }
    #[test]
    fn incomplete_corrupt_and_noncontiguous_frames_never_produce_an_overlay() {
        let data = frame(1, &[Operation::delete(Table::Threads, &[1; 16]).unwrap()]);
        let mut cells = [Cell::EMPTY; 8];
        for length in 1..data.len() {
            assert!(matches!(Overlay::decode(
                &Provider,
                &header(0),
                data.get(..length).unwrap(),
                &mut cells
            ), Err(Error::Frame(DecodeError::Incomplete { required, supplied }))
                if supplied == length && required == if length < FRAME_HEADER_BYTES { FRAME_HEADER_BYTES } else { data.len() }));
        }
        for offset in [0, 32, 64, data.len() - 1] {
            let mut corrupt = data.clone();
            *corrupt.get_mut(offset).unwrap() ^= 1;
            assert!(matches!(
                Overlay::decode(&Provider, &header(0), &corrupt, &mut cells),
                Err(Error::Frame(DecodeError::Invalid(_)))
            ));
        }
        let skipped = frame(2, &[Operation::delete(Table::Threads, &[1; 16]).unwrap()]);
        assert!(Overlay::decode(&Provider, &header(0), &skipped, &mut cells).is_err());
        let mut repeated = data.clone();
        repeated.extend_from_slice(&data);
        assert!(Overlay::decode(&Provider, &header(0), &repeated, &mut cells).is_err());
        assert!(Overlay::decode(&Provider, &header(u64::MAX), &data, &mut cells).is_err());
        let overlay = Overlay::decode(&Provider, &header(0), &data, &mut cells).unwrap();
        assert_eq!(overlay.operation_count(), 1);
    }
    #[test]
    fn byte_and_slot_bounds_charge_all_operations_including_duplicate_keys() {
        let operation = Operation::delete(Table::Threads, &[1; 16]).unwrap();
        let data = frame(1, &[operation]);
        assert!(Overlay::decode(&Provider, &header(0), &data, &mut []).is_err());
        let mut oversized = vec![Cell::EMPTY; MAX_JOURNAL_OPERATIONS + 1];
        assert!(Overlay::decode(&Provider, &header(0), &[], &mut oversized).is_err());
        assert!(Overlay::decode(
            &Provider,
            &header(0),
            &vec![0; MAX_JOURNAL_FRAME_BYTES + 1],
            &mut []
        )
        .is_err());
        let operations = vec![operation; format::MAX_FRAME_OPERATIONS];
        let mut bytes = frame(1, &operations);
        bytes.extend_from_slice(&frame(2, &operations));
        let mut cells = vec![Cell::EMPTY; MAX_JOURNAL_OPERATIONS];
        let overlay = Overlay::decode(&Provider, &header(0), &bytes, &mut cells).unwrap();
        assert_eq!(overlay.operation_count(), 8192);
        assert_eq!(overlay.get(thread(1)).unwrap().unwrap().ordinal, 4095);
        assert_eq!(
            overlay.get(thread(1)).unwrap().unwrap().sequence,
            Sequence::from_u64(2)
        );
        bytes.extend_from_slice(&frame(3, &[operation]));
        assert!(Overlay::decode(&Provider, &header(0), &bytes, &mut cells).is_err());
        let empty = Overlay::decode(&Provider, &header(u64::MAX), &[], &mut []).unwrap();
        assert_eq!(empty.operation_count(), 0);
        assert!(empty.get(thread(1)).unwrap().is_none());
        assert!(empty.next(Table::Threads, None).unwrap().is_none());
        assert_eq!(empty.summary().through(), Sequence::from_u64(u64::MAX));
        assert!(std::mem::size_of::<Cell>() <= crate::limits::JOURNAL_SLOT_BYTES);
    }
}
