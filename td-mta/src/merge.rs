//! Provisional sorted table replay; publication and whole-file proofs stay external.
use crate::{
    format::{
        self,
        table::{Record, TableHeader},
        MAX_KEY_BYTES,
    },
    overlay::{Entry, Overlay},
    ports::{Mutation, Record as RowRecord},
};

#[derive(Debug, Eq, PartialEq)]
pub enum Error<E> {
    Format(format::Error),
    Sink(E),
    Failed,
}
impl<E> From<format::Error> for Error<E> {
    fn from(value: format::Error) -> Self {
        Self::Format(value)
    }
}
impl<E: std::fmt::Display> std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Format(e) => write!(f, "table merge input: {e}"),
            Self::Sink(e) => write!(f, "table merge sink: {e}"),
            Self::Failed => f.write_str("table merge already failed"),
        }
    }
}
impl<E: std::error::Error + 'static> std::error::Error for Error<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Format(e) => Some(e),
            Self::Sink(e) => Some(e),
            Self::Failed => None,
        }
    }
}

/// One supplied table's provisional rows. Sink effects must remain unpublished
/// until table completion/binding and complete final-view validation succeed.
pub struct Merge<'o, 'b, 's> {
    overlay: &'o Overlay<'b, 's>,
    header: TableHeader,
    pending: Option<Entry<'b>>,
    previous: [u8; MAX_KEY_BYTES],
    previous_len: usize,
    records: u64,
    payload: u64,
    emitted: u64,
    failed: bool,
}
impl<'o, 'b, 's> Merge<'o, 'b, 's> {
    pub fn new(header: TableHeader, overlay: &'o Overlay<'b, 's>) -> Result<Self, format::Error> {
        header.validate()?;
        let journal = overlay.summary().header();
        if header.account != journal.account
            || header.epoch != journal.epoch
            || header.through != journal.base
        {
            return Err(format::Error::InvalidValue);
        }
        Ok(Self {
            overlay,
            header,
            pending: overlay.next(header.table, None)?,
            previous: [0; MAX_KEY_BYTES],
            previous_len: 0,
            records: 0,
            payload: 0,
            emitted: 0,
            failed: false,
        })
    }
    pub const fn is_failed(&self) -> bool {
        self.failed
    }
    /// Admit one checkpoint record plus up to 8192 intervening overlay keys.
    /// Every returned error poisons this merge, including errors from the sink.
    pub fn push<E>(
        &mut self,
        record: Record<'_>,
        mut sink: impl FnMut(RowRecord<'_>) -> Result<(), E>,
    ) -> Result<(), Error<E>> {
        if self.failed {
            return Err(Error::Failed);
        }
        self.failed = true;
        let records = self.records.checked_add(1).ok_or(format::Error::Overflow)?;
        let length = u64::try_from(record.encoded_len()?).map_err(|_| format::Error::Overflow)?;
        let payload = self
            .payload
            .checked_add(length)
            .ok_or(format::Error::Overflow)?;
        let key = record.key_bytes();
        // A locally valid Record need not have passed a file stream verifier.
        if records > self.header.record_count || payload > self.header.payload_bytes {
            return Err(format::Error::TrailingBytes.into());
        }
        if record.row().key.table() != self.header.table
            || record.row().last_change > self.header.through
            || (self.records != 0
                && self
                    .previous
                    .get(..self.previous_len)
                    .ok_or(format::Error::InvalidValue)?
                    >= key)
        {
            return Err(format::Error::InvalidValue.into());
        }
        let mut replaced = false;
        let mut encoded = [0; MAX_KEY_BYTES];
        while let Some(entry) = self.pending {
            let pending_key = match entry.mutation {
                Mutation::Put { key, .. } | Mutation::Delete(key) => key,
            };
            let length = pending_key.encode(&mut encoded)?;
            let encoded = encoded.get(..length).ok_or(format::Error::InvalidValue)?;
            match encoded.cmp(key) {
                std::cmp::Ordering::Greater => break,
                std::cmp::Ordering::Equal => {
                    self.emit(entry, &mut sink)?;
                    self.pending = self.overlay.next(self.header.table, Some(encoded))?;
                    replaced = true;
                    break;
                }
                std::cmp::Ordering::Less => {
                    self.emit(entry, &mut sink)?;
                    self.pending = self.overlay.next(self.header.table, Some(encoded))?;
                }
            }
        }
        if !replaced {
            self.emit_row(record.row(), &mut sink)?;
        }
        self.previous
            .get_mut(..key.len())
            .ok_or(format::Error::Limit)?
            .copy_from_slice(key);
        self.previous_len = key.len();
        self.records = records;
        self.payload = payload;
        self.failed = false;
        Ok(())
    }
    fn emit<E>(
        &mut self,
        entry: Entry<'_>,
        sink: &mut impl FnMut(RowRecord<'_>) -> Result<(), E>,
    ) -> Result<(), Error<E>> {
        if let Mutation::Put { key, row } = entry.mutation {
            self.emit_row(
                RowRecord {
                    key,
                    row,
                    last_change: entry.sequence,
                },
                sink,
            )?;
        }
        Ok(())
    }
    fn emit_row<E>(
        &mut self,
        row: RowRecord<'_>,
        sink: &mut impl FnMut(RowRecord<'_>) -> Result<(), E>,
    ) -> Result<(), Error<E>> {
        let emitted = self.emitted.checked_add(1).ok_or(format::Error::Overflow)?;
        sink(row).map_err(Error::Sink)?;
        self.emitted = emitted;
        Ok(())
    }
    /// Drain remaining overlay keys after exactly the declared table input.
    /// Returns the live row count only, not an EOF, digest or publication proof.
    pub fn finish<E>(
        mut self,
        mut sink: impl FnMut(RowRecord<'_>) -> Result<(), E>,
    ) -> Result<u64, Error<E>> {
        if self.failed {
            return Err(Error::Failed);
        }
        if self.records != self.header.record_count || self.payload != self.header.payload_bytes {
            return Err(format::Error::Truncated.into());
        }
        let mut encoded = [0; MAX_KEY_BYTES];
        while let Some(entry) = self.pending {
            self.emit(entry, &mut sink)?;
            let key = match entry.mutation {
                Mutation::Put { key, .. } | Mutation::Delete(key) => key,
            };
            let length = key.encode(&mut encoded)?;
            self.pending = self.overlay.next(
                self.header.table,
                Some(encoded.get(..length).ok_or(format::Error::InvalidValue)?),
            )?;
        }
        Ok(self.emitted)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{
        format::{
            container::JournalHeader,
            frame,
            operation::Operation,
            row::{BlobKind, BlobRow, Row},
            Sequence, Table, FRAME_FOOTER_BYTES, FRAME_HEADER_BYTES, JOURNAL_HEADER_BYTES,
        },
        ids::{AccountId, StoreEpoch},
        overlay::Cell,
    };
    use td_crypto::Provider;
    const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
    const EPOCH: StoreEpoch = StoreEpoch::from_bytes([2; 16]);
    fn header(count: u64) -> TableHeader {
        TableHeader {
            table: Table::Blobs,
            account: ACCOUNT,
            epoch: EPOCH,
            generation: 1,
            through: Sequence::from_u64(5),
            record_count: count,
            payload_bytes: count * 113,
        }
    }
    fn journal() -> [u8; JOURNAL_HEADER_BYTES] {
        let mut b = [0; JOURNAL_HEADER_BYTES];
        JournalHeader {
            account: ACCOUNT,
            epoch: EPOCH,
            segment: 2,
            base: Sequence::from_u64(5),
        }
        .encode(&Provider, &mut b)
        .unwrap();
        b
    }
    fn value(n: u8) -> [u8; 49] {
        let mut b = [0; 49];
        Row::Blob(BlobRow {
            kind: BlobKind::Message,
            length: u64::from(n),
            digest: [n; 32],
            created_at: i64::from(n),
        })
        .encode(&mut b)
        .unwrap();
        b
    }
    fn frame(sequence: u64, ops: &[Operation<'_>]) -> Vec<u8> {
        let mut b = vec![
            0;
            FRAME_HEADER_BYTES
                + FRAME_FOOTER_BYTES
                + ops.iter().map(|v| v.encoded_len().unwrap()).sum::<usize>()
        ];
        let mut offset = FRAME_HEADER_BYTES;
        for op in ops {
            offset += op.encode(b.get_mut(offset..).unwrap()).unwrap();
        }
        frame::seal(&Provider, Sequence::from_u64(sequence), ops.len(), &mut b).unwrap();
        b
    }
    fn record<'a>(key: &'a [u8], value: &'a [u8]) -> Record<'a> {
        Record::new(Table::Blobs, Sequence::from_u64(3), key, value).unwrap()
    }
    fn own(row: RowRecord<'_>) -> (u8, u64, u64) {
        let id = match row.key {
            crate::format::key::Key::Blob(v) => *v.as_bytes().first().unwrap(),
            _ => panic!("wrong table"),
        };
        let value = match row.row {
            Row::Blob(v) => v.length,
            _ => panic!("wrong row"),
        };
        (id, value, row.last_change.number())
    }
    #[test]
    fn sorted_union_replaces_rows_preserves_sequences_and_suppresses_tombstones() {
        let v = value(11);
        let other = value(44);
        let ops = [
            Operation::put(Table::Blobs, &[1; 16], &v).unwrap(),
            Operation::delete(Table::Blobs, &[2; 16]).unwrap(),
            Operation::put(Table::Blobs, &[4; 16], &v).unwrap(),
            Operation::put(Table::Blobs, &[4; 16], &other).unwrap(),
            Operation::delete(Table::Blobs, &[5; 16]).unwrap(),
            Operation::put(Table::Blobs, &[9; 16], &v).unwrap(),
        ];
        let bytes = frame(6, &ops);
        let mut cells = [Cell::EMPTY; 6];
        let overlay = Overlay::decode(&Provider, &journal(), &bytes, &mut cells).unwrap();
        let mut merge = Merge::new(header(4), &overlay).unwrap();
        let mut out = Vec::new();
        for id in [2, 4, 6, 8] {
            merge
                .push(record(&[id; 16], &value(id)), |r| {
                    out.push(own(r));
                    Ok::<_, ()>(())
                })
                .unwrap();
        }
        let rows = merge
            .finish(|r| {
                out.push(own(r));
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(rows, 5);
        assert_eq!(
            out,
            vec![(1, 11, 6), (4, 44, 6), (6, 6, 3), (8, 8, 3), (9, 11, 6)]
        );
    }
    #[test]
    fn replay_matches_independent_map_with_cross_frame_updates() {
        use std::collections::BTreeMap;
        let mut expected = BTreeMap::new();
        let mut keys = Vec::new();
        let mut values = Vec::new();
        for id in (0u8..96).step_by(3) {
            keys.push([id; 16]);
            values.push(value(id));
            expected.insert(id, (u64::from(id), 3));
        }
        let mut frames = Vec::new();
        let mut operation_count = 0;
        for sequence in 6..=9 {
            let keys: Vec<_> = (0..60)
                .map(|i| [((i * 13 + sequence * 7) % 101) as u8; 16])
                .collect();
            let values: Vec<_> = (0..60).map(|i| value((i + sequence) as u8)).collect();
            let mut ops = Vec::new();
            for (i, (key, value)) in keys.iter().zip(&values).enumerate() {
                let id = *key.first().unwrap();
                if i % 3 == 0 {
                    ops.push(Operation::delete(Table::Blobs, key).unwrap());
                    expected.remove(&id);
                } else {
                    ops.push(Operation::put(Table::Blobs, key, value).unwrap());
                    expected.insert(id, ((i + sequence as usize) as u64, sequence));
                }
            }
            operation_count += ops.len();
            frames.extend_from_slice(&frame(sequence, &ops));
        }
        let mut cells = vec![Cell::EMPTY; operation_count];
        let overlay = Overlay::decode(&Provider, &journal(), &frames, &mut cells).unwrap();
        let mut merge = Merge::new(header(keys.len() as u64), &overlay).unwrap();
        let mut actual = Vec::new();
        for (key, value) in keys.iter().zip(&values) {
            merge
                .push(record(key, value), |r| {
                    actual.push(own(r));
                    Ok::<_, ()>(())
                })
                .unwrap();
        }
        let count = merge
            .finish(|r| {
                actual.push(own(r));
                Ok::<_, ()>(())
            })
            .unwrap();
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(id, (v, s))| (id, v, s))
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(count, actual.len() as u64);
    }
    #[test]
    fn input_mismatch_and_sink_failure_permanently_poison_merge() {
        let mut cells = [];
        let overlay = Overlay::decode(&Provider, &journal(), &[], &mut cells).unwrap();
        for mode in 0..4 {
            let mut merge = Merge::new(header(2), &overlay).unwrap();
            merge
                .push(record(&[2; 16], &value(2)), |_| Ok::<_, ()>(()))
                .unwrap();
            let value = value(1);
            let r = match mode {
                0 => record(&[1; 16], &value),
                1 => record(&[2; 16], &value),
                2 => Record::new(Table::Blobs, Sequence::from_u64(6), &[3; 16], &value).unwrap(),
                _ => Record::new(Table::Threads, Sequence::from_u64(3), &[3; 16], &[]).unwrap(),
            };
            assert!(matches!(
                merge.push(r, |_| Ok::<_, ()>(())),
                Err(Error::Format(_))
            ));
            assert!(merge.is_failed());
            assert_eq!(
                merge.push(record(&[4; 16], &value), |_| Ok::<_, ()>(())),
                Err(Error::Failed)
            );
            assert_eq!(merge.finish(|_| Ok::<_, ()>(())), Err(Error::Failed));
        }
        let mut merge = Merge::new(header(1), &overlay).unwrap();
        assert_eq!(
            merge.push(record(&[2; 16], &value(2)), |_| Err("sink")),
            Err(Error::Sink("sink"))
        );
        assert!(merge.is_failed());
        assert_eq!(merge.finish(|_| Ok::<_, &str>(())), Err(Error::Failed));
        let mut merge = Merge::new(header(0), &overlay).unwrap();
        assert!(matches!(
            merge.push(record(&[2; 16], &value(2)), |_| Ok::<_, ()>(())),
            Err(Error::Format(_))
        ));
    }
    #[test]
    fn identities_counts_empty_tables_and_remaining_sink_failure_are_checked() {
        let v = value(7);
        let bytes = frame(6, &[Operation::put(Table::Blobs, &[1; 16], &v).unwrap()]);
        let mut cells = [Cell::EMPTY];
        let overlay = Overlay::decode(&Provider, &journal(), &bytes, &mut cells).unwrap();
        let h = header(0);
        for wrong in [
            TableHeader {
                account: AccountId::from_bytes([9; 16]),
                ..h
            },
            TableHeader {
                epoch: StoreEpoch::from_bytes([9; 16]),
                ..h
            },
            TableHeader {
                through: Sequence::from_u64(4),
                ..h
            },
            TableHeader { generation: 0, ..h },
            TableHeader {
                payload_bytes: 1,
                ..h
            },
        ] {
            assert!(Merge::new(wrong, &overlay).is_err());
        }
        assert_eq!(
            Merge::new(h, &overlay).unwrap().finish(|_| Err("sink")),
            Err(Error::Sink("sink"))
        );
        assert_eq!(
            Merge::new(header(1), &overlay)
                .unwrap()
                .finish(|_| Ok::<_, ()>(())),
            Err(Error::Format(format::Error::Truncated))
        );
        let mut calls = 0;
        assert_eq!(
            Merge::new(h, &overlay)
                .unwrap()
                .finish(|r| {
                    assert_eq!(own(r), (1, 7, 6));
                    calls += 1;
                    Ok::<_, ()>(())
                })
                .unwrap(),
            1
        );
        assert_eq!(calls, 1);
        let other = TableHeader {
            table: Table::Threads,
            ..h
        };
        assert_eq!(
            Merge::new(other, &overlay)
                .unwrap()
                .finish(|_| Ok::<_, ()>(()))
                .unwrap(),
            0
        );
        let bad_payload = TableHeader {
            payload_bytes: 114,
            ..header(1)
        };
        let mut merge = Merge::new(bad_payload, &overlay).unwrap();
        merge
            .push(record(&[2; 16], &v), |_| Ok::<_, ()>(()))
            .unwrap();
        assert_eq!(
            merge.finish(|_| Ok::<_, ()>(())),
            Err(Error::Format(format::Error::Truncated))
        );
    }
    #[test]
    fn variable_and_maximum_keys_follow_encoded_order() {
        use crate::{format::key::Key, ids::EmailId};
        let maximum = "a".repeat(crate::format::key::MAX_ANCHOR_BYTES);
        let encode = |text: &str| {
            let key = Key::ThreadAnchor(text, EmailId::from_bytes([1; 16]));
            let mut bytes = vec![0; key.encoded_len().unwrap()];
            key.encode(&mut bytes).unwrap();
            bytes
        };
        let max_key = encode(&maximum);
        assert_eq!(max_key.len(), MAX_KEY_BYTES);
        let inserted = encode("aa");
        let mut checkpoint = vec![encode("a"), encode("b"), max_key.clone()];
        checkpoint.sort();
        let bytes = frame(
            6,
            &[
                Operation::delete(Table::ThreadAnchors, &max_key).unwrap(),
                Operation::put(Table::ThreadAnchors, &inserted, &[]).unwrap(),
            ],
        );
        let mut cells = [Cell::EMPTY; 2];
        let overlay = Overlay::decode(&Provider, &journal(), &bytes, &mut cells).unwrap();
        let payload = checkpoint
            .iter()
            .map(|key| {
                Record::new(Table::ThreadAnchors, Sequence::from_u64(3), key, &[])
                    .unwrap()
                    .encoded_len()
                    .unwrap() as u64
            })
            .sum();
        let table = TableHeader {
            table: Table::ThreadAnchors,
            record_count: checkpoint.len() as u64,
            payload_bytes: payload,
            ..header(0)
        };
        let mut merge = Merge::new(table, &overlay).unwrap();
        let mut actual = Vec::new();
        let mut sink = |row: RowRecord<'_>| {
            let mut key = vec![0; row.key.encoded_len().unwrap()];
            row.key.encode(&mut key).unwrap();
            actual.push(key);
            Ok::<_, ()>(())
        };
        for key in &checkpoint {
            merge
                .push(
                    Record::new(Table::ThreadAnchors, Sequence::from_u64(3), key, &[]).unwrap(),
                    &mut sink,
                )
                .unwrap();
        }
        assert_eq!(merge.finish(&mut sink).unwrap(), 3);
        checkpoint.retain(|key| key != &max_key);
        checkpoint.push(inserted);
        checkpoint.sort();
        assert_eq!(actual, checkpoint);
    }
    #[test]
    fn independent_count_and_payload_limits_refuse_before_callback_output() {
        assert!(std::mem::size_of::<Merge<'_, '_, '_>>() <= 4096);
        let mut cells = [];
        let overlay = Overlay::decode(&Provider, &journal(), &[], &mut cells).unwrap();
        // The second record fits one ceiling and exceeds only the other.
        for table in [
            TableHeader {
                payload_bytes: 226,
                ..header(1)
            },
            TableHeader {
                payload_bytes: 128,
                ..header(2)
            },
        ] {
            let mut merge = Merge::new(table, &overlay).unwrap();
            merge
                .push(record(&[1; 16], &value(1)), |_| Ok::<_, ()>(()))
                .unwrap();
            let mut calls = 0;
            assert_eq!(
                merge.push(record(&[2; 16], &value(2)), |_| {
                    calls += 1;
                    Ok::<_, ()>(())
                }),
                Err(Error::Format(format::Error::TrailingBytes))
            );
            assert_eq!(calls, 0);
            assert!(merge.is_failed());
        }
    }
}
