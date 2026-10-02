//! Full selected-table scan retaining the first row beyond a cursor.
use super::super::LoadedOverlay;
use super::{CompleteReplay, TableInput, TableInputError, TableReplay, TableReplayError};
use crate::{
    format::{self, key::Key, row::Row, Sequence, Table},
    ports::{Crypto, Record},
};

pub type NextError = TableReplayError<format::Error>;

pub struct TableNext<'r, 'c, 'm, 't, 'o, 'b, 's, 'a, 'v, C: Crypto> {
    replay: TableReplay<'r, 'c, 'm, 't, 'o, 'b, 's, C>,
    capture: Capture<'a, 'v>,
}
struct Capture<'a, 'v> {
    table: Table,
    after: Option<&'a [u8]>,
    key: &'v mut [u8],
    value: &'v mut [u8],
    found: Option<(usize, usize, Sequence)>,
}
impl<'r, 'c, 'm, 't, C: Crypto> TableInput<'r, 'c, 'm, 't, C> {
    /// Admit the full scan and keep cursor/key/value buffers separate from input.
    pub fn into_next<'o, 'b, 's, 'a, 'v>(
        self,
        active: &'o LoadedOverlay<'r, 'b, 's>,
        after: Option<&'a [u8]>,
        key: &'v mut [u8],
        value: &'v mut [u8],
    ) -> Result<TableNext<'r, 'c, 'm, 't, 'o, 'b, 's, 'a, 'v, C>, TableInputError> {
        let table = self.table;
        if let Some(bytes) = after {
            Key::decode(table, bytes)?.validate_local()?;
        }
        Ok(TableNext {
            replay: self.into_replay(active)?,
            capture: Capture {
                table,
                after,
                key,
                value,
                found: None,
            },
        })
    }
}
impl<'r, 'o, 'b, 's, 'v, C: Crypto> TableNext<'r, '_, '_, '_, 'o, 'b, 's, '_, 'v, C> {
    pub fn is_failed(&self) -> bool {
        self.replay.is_failed()
    }
    /// One replay step. Even a captured candidate remains unavailable until finish.
    pub fn advance(&mut self) -> Result<bool, NextError> {
        self.replay.advance(|row| self.capture.accept(row))
    }
    pub fn finish(mut self) -> Result<CompleteNext<'r, 'o, 'b, 's, 'v>, NextError> {
        let replay = self.replay.finish(|row| self.capture.accept(row))?;
        let row = self
            .capture
            .finish()
            .map_err(|e| TableReplayError::Merge(crate::merge::Error::Sink(e)))?;
        Ok(CompleteNext { replay, row })
    }
}
impl<'v> Capture<'_, 'v> {
    fn accept(&mut self, record: Record<'_>) -> Result<(), format::Error> {
        if self.found.is_some() {
            return Ok(());
        }
        let mut encoded = [0; format::MAX_KEY_BYTES];
        let key_len = record.key.encode(&mut encoded)?;
        let bytes = encoded.get(..key_len).ok_or(format::Error::Truncated)?;
        if self.after.is_some_and(|after| bytes <= after) {
            return Ok(());
        }
        let key_out = self
            .key
            .get_mut(..key_len)
            .ok_or(format::Error::OutputFull)?;
        let value_len = record.row.encode(self.value)?;
        key_out.copy_from_slice(bytes);
        self.found = Some((key_len, value_len, record.last_change));
        Ok(())
    }
    fn finish(self) -> Result<Option<Record<'v>>, format::Error> {
        let Some((key_len, value_len, last_change)) = self.found else {
            return Ok(None);
        };
        let key = Key::decode(
            self.table,
            self.key.get(..key_len).ok_or(format::Error::Truncated)?,
        )?;
        let row = Row::decode(
            self.table,
            self.value
                .get(..value_len)
                .ok_or(format::Error::Truncated)?,
        )?;
        row.validate_key(key)?;
        Ok(Some(Record {
            key,
            row,
            last_change,
        }))
    }
}
/// First final row after the cursor (or proven exhaustion) for this table/prefix.
/// Cross-row references and actual pins still require coordinator validation.
pub struct CompleteNext<'r, 'o, 'b, 's, 'v> {
    replay: CompleteReplay<'r, 'o, 'b, 's>,
    row: Option<Record<'v>>,
}
impl<'r, 'o, 'b, 's, 'v> CompleteNext<'r, 'o, 'b, 's, 'v> {
    pub fn replay(&self) -> &CompleteReplay<'r, 'o, 'b, 's> {
        &self.replay
    }
    pub fn row(&self) -> Option<Record<'v>> {
        self.row
    }
    pub fn into_parts(self) -> (CompleteReplay<'r, 'o, 'b, 's>, Option<Record<'v>>) {
        (self.replay, self.row)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) fn probe(
    root: &super::super::LockedRoot,
    table: &super::ProbeBytes,
    active: &super::super::active::ProbeBytes,
    frames: &mut [u8],
    cells: &mut [crate::overlay::Cell],
    scratch: &mut [u8; crate::format::table::MAX_RECORD_BYTES],
    output: &mut [u8],
) {
    use crate::ids::BlobId;
    let mut key = [0; format::MAX_KEY_BYTES];
    for (through, end, after, present) in [
        (1, 96, None, true),
        (1, 96, Some(0x33), true),
        (1, 96, Some(0x44), false),
        (1, 96, Some(0x55), false),
        (2, 256, None, false),
    ] {
        let loaded = root
            .load_active_overlay(
                &td_crypto::Provider,
                active.selection(),
                active.view(through, end),
                end,
                frames,
                cells,
            )
            .unwrap();
        let cursor = after.map(|byte| [byte; 16]);
        let mut next = root
            .open_table(
                &td_crypto::Provider,
                table.selection(),
                Table::Blobs,
                225,
                scratch,
            )
            .unwrap()
            .into_next(
                &loaded,
                cursor.as_ref().map(|a| a.as_slice()),
                &mut key,
                output,
            )
            .unwrap();
        assert!(next.advance().unwrap());
        assert!(!next.advance().unwrap());
        let done = next.finish().unwrap();
        assert_eq!(done.row().is_some(), present);
        if let Some(row) = done.row() {
            assert_eq!(row.key, Key::Blob(BlobId::from_bytes([0x44; 16])));
            assert_eq!(row.last_change, Sequence::from_u64(1));
        }
        assert_eq!(done.replay().identity(), active.view(through, end));
        let row = done.row();
        let (replay, moved) = done.into_parts();
        drop(replay);
        assert_eq!(row, moved);
    }
    let loaded = root
        .load_active_overlay(
            &td_crypto::Provider,
            active.selection(),
            active.view(1, 96),
            96,
            frames,
            cells,
        )
        .unwrap();
    let mut next = root
        .open_table(
            &td_crypto::Provider,
            table.selection(),
            Table::Blobs,
            225,
            scratch,
        )
        .unwrap()
        .into_next(&loaded, None, &mut [], output)
        .unwrap();
    assert!(matches!(
        next.advance(),
        Err(TableReplayError::Merge(crate::merge::Error::Sink(
            format::Error::OutputFull
        )))
    ));
    assert!(next.is_failed());
    assert!(matches!(next.advance(), Err(TableReplayError::Failed)));
    assert!(matches!(next.finish(), Err(TableReplayError::Failed)));
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::super::{active, tests::Fixture, LockedRoot};
    use super::super::{fixture, MAX_RECORD_BYTES};
    use super::*;
    use crate::{
        format::{frame, operation::Operation, Table, FRAME_FOOTER_BYTES, FRAME_HEADER_BYTES},
        ids::{BlobId, EmailId},
        overlay::Cell,
        store_paths::{AccountEntry, Number},
    };
    use td_crypto::{Digest, Provider};
    fn setup(f: &Fixture) -> (LockedRoot, fixture::Bytes, active::ProbeBytes) {
        let root = f.locked();
        root.create_accounts_directory().unwrap();
        for entry in [
            AccountEntry::Root,
            AccountEntry::Metadata,
            AccountEntry::Checkpoints,
            AccountEntry::Checkpoint(Number::new(2).unwrap()),
            AccountEntry::Journals,
        ] {
            root.create_account_directory(fixture::ACCOUNT, entry)
                .unwrap();
        }
        let table = fixture::prepare(&root);
        let active = active::prepare_probe(&root);
        (root, table, active)
    }
    #[test]
    fn exclusive_cursor_handles_first_equal_later_and_deleted_rows() {
        let f = Fixture::new();
        let (root, table, active) = setup(&f);
        let mut scratch = vec![0; MAX_RECORD_BYTES];
        probe(
            &root,
            &table,
            &active,
            &mut [0; 160],
            &mut [Cell::EMPTY; 2],
            scratch.as_mut_slice().try_into().unwrap(),
            &mut [0; 64],
        );
    }
    #[test]
    fn replacements_and_residual_candidates_preserve_sorted_successors() {
        let f = Fixture::new();
        let (root, table, active) = setup(&f);
        let blob = crate::format::row::BlobRow {
            kind: crate::format::row::BlobKind::Message,
            length: 917,
            digest: [0x71; 32],
            created_at: 42,
        };
        let mut bytes = [0; 64];
        let used = Row::Blob(blob).encode(&mut bytes).unwrap();
        for deleted in [false, true] {
            let operations = [
                Operation::put(Table::Blobs, &[0x33; 16], bytes.get(..used).unwrap()).unwrap(),
                if deleted {
                    Operation::delete(Table::Blobs, &[0x44; 16]).unwrap()
                } else {
                    Operation::put(Table::Blobs, &[0x44; 16], bytes.get(..used).unwrap()).unwrap()
                },
                Operation::put(Table::Blobs, &[0x55; 16], bytes.get(..used).unwrap()).unwrap(),
            ];
            let mut frame = vec![
                0;
                FRAME_HEADER_BYTES
                    + FRAME_FOOTER_BYTES
                    + operations
                        .iter()
                        .map(|op| op.encoded_len().unwrap())
                        .sum::<usize>()
            ];
            let mut offset = FRAME_HEADER_BYTES;
            for op in operations {
                offset += op.encode(frame.get_mut(offset..).unwrap()).unwrap();
            }
            frame::seal(&Provider, Sequence::from_u64(2), 3, &mut frame).unwrap();
            let mut journal = active::fixture::journal().get(..96).unwrap().to_vec();
            journal.extend_from_slice(&frame);
            active::fixture::write(&root, &journal);
            let mut frames = vec![0; frame.len()];
            let mut cells = [Cell::EMPTY; 3];
            let loaded = root
                .load_active_overlay(
                    &Provider,
                    active.selection(),
                    active.view(2, journal.len() as u64),
                    journal.len() as u64,
                    &mut frames,
                    &mut cells,
                )
                .unwrap();
            let mut scratch = vec![0; MAX_RECORD_BYTES];
            for (after, expected) in [
                (None, Some(0x33)),
                (Some(0x33), Some(if deleted { 0x55 } else { 0x44 })),
                (Some(0x44), Some(0x55)),
                (Some(0x55), None),
            ] {
                let cursor = after.map(|byte| [byte; 16]);
                let mut key = [0; 16];
                let mut value = [0; 64];
                let mut next = root
                    .open_table(
                        &Provider,
                        table.selection(),
                        Table::Blobs,
                        225,
                        scratch.as_mut_slice().try_into().unwrap(),
                    )
                    .unwrap()
                    .into_next(
                        &loaded,
                        cursor.as_ref().map(|v| v.as_slice()),
                        &mut key,
                        &mut value,
                    )
                    .unwrap();
                while next.advance().unwrap() {}
                let done = next.finish().unwrap();
                assert_eq!(done.replay().rows(), if deleted { 2 } else { 3 });
                assert_eq!(
                    done.row(),
                    expected.map(|byte| Record {
                        key: Key::Blob(BlobId::from_bytes([byte; 16])),
                        row: Row::Blob(blob),
                        last_change: Sequence::from_u64(2)
                    })
                );
            }
            for (key_len, value_len) in [(15, 64), (16, 48)] {
                let mut key = [0; 16];
                let mut value = [0; 64];
                let mut next = root
                    .open_table(
                        &Provider,
                        table.selection(),
                        Table::Blobs,
                        225,
                        scratch.as_mut_slice().try_into().unwrap(),
                    )
                    .unwrap()
                    .into_next(
                        &loaded,
                        Some(&[0x44; 16]),
                        key.get_mut(..key_len).unwrap(),
                        value.get_mut(..value_len).unwrap(),
                    )
                    .unwrap();
                assert!(next.advance().unwrap());
                assert!(!next.advance().unwrap());
                assert!(matches!(
                    next.finish(),
                    Err(TableReplayError::Merge(crate::merge::Error::Sink(
                        format::Error::OutputFull
                    )))
                ));
            }
        }
    }
    #[test]
    fn captured_row_does_not_hide_unread_input_or_late_digest_failure() {
        let f = Fixture::new();
        let (root, mut table, active) = setup(&f);
        let bytes = super::super::tests::two_rows(&root, &mut table);
        let mut frames = [];
        let mut cells = [];
        let loaded = root
            .load_active_overlay(
                &Provider,
                table.selection(),
                active.view(1, 96),
                96,
                &mut frames,
                &mut cells,
            )
            .unwrap();
        let mut scratch = vec![0; MAX_RECORD_BYTES];
        let mut key = [0; 16];
        let mut value = [0; 64];
        let mut next = root
            .open_table(
                &Provider,
                table.selection(),
                Table::Blobs,
                bytes.len() as u64,
                scratch.as_mut_slice().try_into().unwrap(),
            )
            .unwrap()
            .into_next(&loaded, None, &mut key, &mut value)
            .unwrap();
        assert!(next.advance().unwrap());
        assert!(next.capture.found.is_some());
        assert!(
            matches!(next.finish(),Err(TableReplayError::Input(TableInputError::Io(e))) if e.kind()==std::io::ErrorKind::InvalidInput)
        );
        let mut changed = bytes;
        *changed
            .get_mut(format::TABLE_HEADER_BYTES + 16 + 16 + 1 + 8)
            .unwrap() ^= 1;
        let end = format::TABLE_HEADER_BYTES + 113 - 32;
        let mut hash = Provider.sha256().unwrap();
        hash.update(changed.get(format::TABLE_HEADER_BYTES..end).unwrap())
            .unwrap();
        changed
            .get_mut(end..end + 32)
            .unwrap()
            .copy_from_slice(&hash.finish().unwrap());
        fixture::write(&root, Table::Blobs, &changed);
        for after in [None, Some([0x55; 16])] {
            let mut next = root
                .open_table(
                    &Provider,
                    table.selection(),
                    Table::Blobs,
                    changed.len() as u64,
                    scratch.as_mut_slice().try_into().unwrap(),
                )
                .unwrap()
                .into_next(
                    &loaded,
                    after.as_ref().map(|a| a.as_slice()),
                    &mut key,
                    &mut value,
                )
                .unwrap();
            while next.advance().unwrap() {}
            assert!(matches!(
                next.finish(),
                Err(TableReplayError::Input(TableInputError::Container(
                    format::container::Error::Checksum
                )))
            ));
        }
    }
    #[test]
    fn capture_compares_canonical_encoded_keys_and_retains_only_first_candidate() {
        let text256 = "z".repeat(256);
        let text255 = "a".repeat(255);
        let first = Key::ThreadAnchor(&text256, EmailId::from_bytes([1; 16]));
        let second = Key::ThreadAnchor(&text255, EmailId::from_bytes([2; 16]));
        let mut after = [0; 1024];
        let length = first.encode(&mut after).unwrap();
        let mut key = [0; 1024];
        let mut capture = Capture {
            table: Table::ThreadAnchors,
            after: after.get(..length),
            key: &mut key,
            value: &mut [],
            found: None,
        };
        capture
            .accept(Record {
                key: first,
                row: Row::ThreadAnchor,
                last_change: Sequence::from_u64(1),
            })
            .unwrap();
        assert!(capture.found.is_none());
        capture
            .accept(Record {
                key: second,
                row: Row::ThreadAnchor,
                last_change: Sequence::from_u64(2),
            })
            .unwrap();
        let third = Key::ThreadAnchor(&text255, EmailId::from_bytes([3; 16]));
        capture
            .accept(Record {
                key: third,
                row: Row::ThreadAnchor,
                last_change: Sequence::from_u64(3),
            })
            .unwrap();
        assert_eq!(
            capture.finish().unwrap(),
            Some(Record {
                key: second,
                row: Row::ThreadAnchor,
                last_change: Sequence::from_u64(2)
            })
        );
        let longest = "x".repeat(format::key::MAX_ANCHOR_BYTES);
        let key_max = Key::ThreadAnchor(&longest, EmailId::from_bytes([9; 16]));
        let mut key_out = [0; format::MAX_KEY_BYTES];
        let mut maximum = Capture {
            table: Table::ThreadAnchors,
            after: None,
            key: &mut key_out,
            value: &mut [],
            found: None,
        };
        maximum
            .accept(Record {
                key: key_max,
                row: Row::ThreadAnchor,
                last_change: Sequence::from_u64(4),
            })
            .unwrap();
        assert_eq!(
            maximum.finish().unwrap(),
            Some(Record {
                key: key_max,
                row: Row::ThreadAnchor,
                last_change: Sequence::from_u64(4),
            })
        );
    }
    #[test]
    fn cursor_and_value_refusals_empty_completion_and_inline_state_budget() {
        let f = Fixture::new();
        let (root, table, active) = setup(&f);
        let mut frames = [];
        let mut cells = [];
        let loaded = root
            .load_active_overlay(
                &Provider,
                table.selection(),
                active.view(1, 96),
                96,
                &mut frames,
                &mut cells,
            )
            .unwrap();
        let mut scratch = vec![0; MAX_RECORD_BYTES];
        let mut key = [0; format::MAX_KEY_BYTES];
        let mut value = [0; 64];
        let input = root
            .open_table(
                &Provider,
                table.selection(),
                Table::Blobs,
                225,
                scratch.as_mut_slice().try_into().unwrap(),
            )
            .unwrap();
        assert!(matches!(
            input.into_next(&loaded, Some(&[0; 15]), &mut key, &mut value),
            Err(TableInputError::Container(
                format::container::Error::Format(format::Error::Truncated)
            ))
        ));
        let input = root
            .open_table(
                &Provider,
                table.selection(),
                Table::Keywords,
                225,
                scratch.as_mut_slice().try_into().unwrap(),
            )
            .unwrap();
        let mut cursor = [0; 32];
        let len = Key::Keyword(EmailId::from_bytes([1; 16]), "$SEEN")
            .encode(&mut cursor)
            .unwrap();
        assert!(matches!(
            input.into_next(&loaded, cursor.get(..len), &mut key, &mut value),
            Err(TableInputError::Container(
                format::container::Error::Format(format::Error::InvalidValue)
            ))
        ));
        let mut next = root
            .open_table(
                &Provider,
                table.selection(),
                Table::Blobs,
                225,
                scratch.as_mut_slice().try_into().unwrap(),
            )
            .unwrap()
            .into_next(&loaded, None, &mut key, &mut [])
            .unwrap();
        assert!(matches!(
            next.advance(),
            Err(TableReplayError::Merge(crate::merge::Error::Sink(
                format::Error::OutputFull
            )))
        ));
        assert!(next.is_failed());
        assert!(matches!(next.finish(), Err(TableReplayError::Failed)));
        let mut next = root
            .open_table(
                &Provider,
                table.selection(),
                Table::Threads,
                225,
                scratch.as_mut_slice().try_into().unwrap(),
            )
            .unwrap()
            .into_next(&loaded, None, &mut [], &mut [])
            .unwrap();
        assert!(!next.advance().unwrap());
        let empty = next.finish().unwrap();
        assert!(empty.row().is_none());
        assert_eq!(empty.replay().rows(), 0);
        assert!(
            std::mem::size_of::<TableNext<'_, '_, '_, '_, '_, '_, '_, '_, '_, Provider>>() <= 8192
        );
    }
}
