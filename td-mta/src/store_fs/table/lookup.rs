//! Full selected-table scan with one retained result in caller scratch.
use super::super::LoadedOverlay;
use super::{CompleteReplay, TableInput, TableInputError, TableReplay, TableReplayError};
use crate::{
    format::{self, key::Key, row::Row, Sequence},
    ports::{Crypto, Record},
};

pub type LookupError = TableReplayError<format::Error>;

pub struct TableLookup<'r, 'c, 'm, 't, 'o, 'b, 's, 'k, 'v, C: Crypto> {
    replay: TableReplay<'r, 'c, 'm, 't, 'o, 'b, 's, C>,
    capture: Capture<'k, 'v>,
}
struct Capture<'k, 'v> {
    key: Key<'k>,
    value: &'v mut [u8],
    found: Option<(usize, Sequence)>,
}
impl<'r, 'c, 'm, 't, C: Crypto> TableInput<'r, 'c, 'm, 't, C> {
    /// Admit the full scan and keep key/result storage separate from input scratch.
    /// A short result buffer fails only if the matching row does not fit.
    pub fn into_lookup<'o, 'b, 's, 'k, 'v>(
        self,
        active: &'o LoadedOverlay<'r, 'b, 's>,
        key: Key<'k>,
        value: &'v mut [u8],
    ) -> Result<TableLookup<'r, 'c, 'm, 't, 'o, 'b, 's, 'k, 'v, C>, TableInputError> {
        key.validate_local()?;
        if key.table() != self.table {
            return Err(format::Error::InvalidValue.into());
        }
        Ok(TableLookup {
            replay: self.into_replay(active)?,
            capture: Capture {
                key,
                value,
                found: None,
            },
        })
    }
}
impl<'r, 'o, 'b, 's, 'v, C: Crypto> TableLookup<'r, '_, '_, '_, 'o, 'b, 's, '_, 'v, C> {
    pub fn is_failed(&self) -> bool {
        self.replay.is_failed()
    }
    /// One replay step. Even a captured match remains unavailable until finish.
    pub fn advance(&mut self) -> Result<bool, LookupError> {
        self.replay.advance(|row| self.capture.accept(row))
    }
    pub fn finish(mut self) -> Result<CompleteLookup<'r, 'o, 'b, 's, 'v>, LookupError> {
        let replay = self.replay.finish(|row| self.capture.accept(row))?;
        let row = self
            .capture
            .finish()
            .map_err(|e| TableReplayError::Merge(crate::merge::Error::Sink(e)))?;
        Ok(CompleteLookup { replay, row })
    }
}
impl<'v> Capture<'_, 'v> {
    fn accept(&mut self, record: Record<'_>) -> Result<(), format::Error> {
        if record.key == self.key {
            if self.found.is_some() {
                return Err(format::Error::InvalidValue);
            }
            let length = record.row.encode(self.value)?;
            self.found = Some((length, record.last_change));
        }
        Ok(())
    }
    fn finish(self) -> Result<Option<(Row<'v>, Sequence)>, format::Error> {
        let Some((length, sequence)) = self.found else {
            return Ok(None);
        };
        let bytes = self.value.get(..length).ok_or(format::Error::Truncated)?;
        let row = Row::decode(self.key.table(), bytes)?;
        row.validate_key(self.key)?;
        Ok(Some((row, sequence)))
    }
}
/// One final row (or proven absence) for this table/prefix. Cross-row
/// references and actual view pins still require coordinator validation.
pub struct CompleteLookup<'r, 'o, 'b, 's, 'v> {
    replay: CompleteReplay<'r, 'o, 'b, 's>,
    row: Option<(Row<'v>, Sequence)>,
}
impl<'r, 'o, 'b, 's, 'v> CompleteLookup<'r, 'o, 'b, 's, 'v> {
    pub fn replay(&self) -> &CompleteReplay<'r, 'o, 'b, 's> {
        &self.replay
    }
    pub fn row(&self) -> Option<(Row<'v>, Sequence)> {
        self.row
    }
    pub fn into_parts(self) -> (CompleteReplay<'r, 'o, 'b, 's>, Option<(Row<'v>, Sequence)>) {
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
    use crate::{format::Table, ids::BlobId};
    for (through, end, byte, present) in [
        (1, 96, 0x44, true),
        (1, 96, 0x45, false),
        (2, 256, 0x44, false),
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
        let mut lookup = root
            .open_table(
                &td_crypto::Provider,
                table.selection(),
                Table::Blobs,
                225,
                scratch,
            )
            .unwrap()
            .into_lookup(&loaded, Key::Blob(BlobId::from_bytes([byte; 16])), output)
            .unwrap();
        assert!(lookup.advance().unwrap());
        assert!(!lookup.advance().unwrap());
        let done = lookup.finish().unwrap();
        assert_eq!(done.row().is_some(), present);
        if let Some((row, sequence)) = done.row() {
            assert!(matches!(row, Row::Blob(_)));
            assert_eq!(sequence, Sequence::from_u64(1));
        }
        assert_eq!(done.replay().identity(), active.view(through, end));
        let row = done.row();
        let (replay, moved_row) = done.into_parts();
        assert_eq!(replay.identity(), active.view(through, end));
        drop(replay);
        assert_eq!(row, moved_row);
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
    let mut lookup = root
        .open_table(
            &td_crypto::Provider,
            table.selection(),
            Table::Blobs,
            225,
            scratch,
        )
        .unwrap()
        .into_lookup(&loaded, Key::Blob(BlobId::from_bytes([0x44; 16])), &mut [])
        .unwrap();
    assert!(matches!(
        lookup.advance(),
        Err(TableReplayError::Merge(crate::merge::Error::Sink(
            format::Error::OutputFull
        )))
    ));
    assert!(lookup.is_failed());
    assert!(matches!(lookup.advance(), Err(TableReplayError::Failed)));
    assert!(matches!(lookup.finish(), Err(TableReplayError::Failed)));
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
    fn checkpoint_match_absence_and_tombstone_complete_without_allocation() {
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
    fn replacements_and_residual_puts_return_latest_values_and_sequences() {
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
        let operations = [
            Operation::put(Table::Blobs, &[0x33; 16], bytes.get(..used).unwrap()).unwrap(),
            Operation::put(Table::Blobs, &[0x44; 16], bytes.get(..used).unwrap()).unwrap(),
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
        for byte in [0x33, 0x44, 0x55, 0x66] {
            let mut output = [0; 64];
            let mut lookup = root
                .open_table(
                    &Provider,
                    table.selection(),
                    Table::Blobs,
                    225,
                    scratch.as_mut_slice().try_into().unwrap(),
                )
                .unwrap()
                .into_lookup(
                    &loaded,
                    Key::Blob(BlobId::from_bytes([byte; 16])),
                    output.get_mut(..if byte == 0x66 { 0 } else { 64 }).unwrap(),
                )
                .unwrap();
            while lookup.advance().unwrap() {}
            let done = lookup.finish().unwrap();
            assert_eq!(done.replay().rows(), 3);
            assert_eq!(
                done.row(),
                if byte == 0x66 {
                    None
                } else {
                    Some((Row::Blob(blob), Sequence::from_u64(2)))
                }
            );
        }
        let mut short = [0; 48];
        let mut lookup = root
            .open_table(
                &Provider,
                table.selection(),
                Table::Blobs,
                225,
                scratch.as_mut_slice().try_into().unwrap(),
            )
            .unwrap()
            .into_lookup(
                &loaded,
                Key::Blob(BlobId::from_bytes([0x55; 16])),
                &mut short,
            )
            .unwrap();
        assert!(lookup.advance().unwrap());
        assert!(!lookup.advance().unwrap());
        assert!(matches!(
            lookup.finish(),
            Err(TableReplayError::Merge(crate::merge::Error::Sink(
                format::Error::OutputFull
            )))
        ));
    }
    #[test]
    fn early_finish_wrong_key_and_late_corruption_cannot_return_a_match() {
        let f = Fixture::new();
        let (root, table, active) = setup(&f);
        let mut frames = [];
        let mut cells = [];
        let loaded = root
            .load_active_overlay(
                &Provider,
                active.selection(),
                active.view(1, 96),
                96,
                &mut frames,
                &mut cells,
            )
            .unwrap();
        let mut scratch = vec![0; MAX_RECORD_BYTES];
        let mut output = [0; 64];
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
            input.into_lookup(
                &loaded,
                Key::Email(EmailId::from_bytes([0x44; 16])),
                &mut output,
            ),
            Err(TableInputError::Container(
                format::container::Error::Format(format::Error::InvalidValue)
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
        assert!(matches!(
            input.into_lookup(
                &loaded,
                Key::Keyword(EmailId::from_bytes([0x44; 16]), "$SEEN"),
                &mut output,
            ),
            Err(TableInputError::Container(
                format::container::Error::Format(format::Error::InvalidValue)
            ))
        ));
        let input = root
            .open_table(
                &Provider,
                table.selection(),
                Table::Blobs,
                225,
                scratch.as_mut_slice().try_into().unwrap(),
            )
            .unwrap();
        let lookup = input
            .into_lookup(
                &loaded,
                Key::Blob(BlobId::from_bytes([0x44; 16])),
                &mut output,
            )
            .unwrap();
        assert!(matches!(lookup.finish(), Err(TableReplayError::Input(_))));
        let mut changed = fixture::hex(fixture::TABLES.first().unwrap());
        *changed
            .get_mut(format::TABLE_HEADER_BYTES + 16 + 16 + 1 + 8)
            .unwrap() ^= 1;
        let end = changed.len() - 32;
        let mut hash = Provider.sha256().unwrap();
        hash.update(changed.get(format::TABLE_HEADER_BYTES..end).unwrap())
            .unwrap();
        changed
            .get_mut(end..)
            .unwrap()
            .copy_from_slice(&hash.finish().unwrap());
        fixture::write(&root, Table::Blobs, &changed);
        for byte in [0x44, 0x66] {
            let input = root
                .open_table(
                    &Provider,
                    table.selection(),
                    Table::Blobs,
                    225,
                    scratch.as_mut_slice().try_into().unwrap(),
                )
                .unwrap();
            let mut lookup = input
                .into_lookup(
                    &loaded,
                    Key::Blob(BlobId::from_bytes([byte; 16])),
                    &mut output,
                )
                .unwrap();
            assert!(lookup.advance().unwrap());
            assert!(!lookup.advance().unwrap());
            assert!(matches!(
                lookup.finish(),
                Err(TableReplayError::Input(
                    super::super::TableInputError::Container(format::container::Error::Checksum)
                ))
            ));
        }
    }
    #[test]
    fn captured_match_does_not_allow_finish_with_unread_records() {
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
        let mut output = [0; 64];
        let mut lookup = root
            .open_table(
                &Provider,
                table.selection(),
                Table::Blobs,
                bytes.len() as u64,
                scratch.as_mut_slice().try_into().unwrap(),
            )
            .unwrap()
            .into_lookup(
                &loaded,
                Key::Blob(BlobId::from_bytes([0x44; 16])),
                &mut output,
            )
            .unwrap();
        assert!(lookup.advance().unwrap());
        assert!(lookup.capture.found.is_some());
        assert!(matches!(lookup.finish(), Err(TableReplayError::Input(
            TableInputError::Io(e)
        )) if e.kind() == std::io::ErrorKind::InvalidInput));
    }
    #[test]
    fn capture_keeps_variable_rows_zero_length_values_and_exact_key_identity() {
        use crate::format::row::MailboxRow;
        use crate::ids::MailboxId;
        let id = MailboxId::from_bytes([1; 16]);
        let name = "x".repeat(1024);
        let row = Row::Mailbox(MailboxRow {
            name: &name,
            parent: None,
            role: None,
            sort_order: 7,
            subscribed: true,
        });
        let mut output = vec![0; row.encoded_len().unwrap()];
        let mut capture = Capture {
            key: Key::Mailbox(id),
            value: &mut output,
            found: None,
        };
        capture
            .accept(Record {
                key: Key::Mailbox(MailboxId::from_bytes([2; 16])),
                row,
                last_change: Sequence::from_u64(1),
            })
            .unwrap();
        assert!(capture.found.is_none());
        capture
            .accept(Record {
                key: Key::Mailbox(id),
                row,
                last_change: Sequence::from_u64(8),
            })
            .unwrap();
        assert_eq!(
            capture.accept(Record {
                key: Key::Mailbox(id),
                row,
                last_change: Sequence::from_u64(8),
            }),
            Err(format::Error::InvalidValue)
        );
        assert_eq!(
            capture.finish().unwrap(),
            Some((row, Sequence::from_u64(8)))
        );
        let anchor = "a".repeat(crate::format::key::MAX_ANCHOR_BYTES);
        let key = Key::ThreadAnchor(&anchor, EmailId::from_bytes([7; 16]));
        let mut capture = Capture {
            key,
            value: &mut [],
            found: None,
        };
        capture
            .accept(Record {
                key,
                row: Row::ThreadAnchor,
                last_change: Sequence::from_u64(9),
            })
            .unwrap();
        assert_eq!(
            capture.finish().unwrap(),
            Some((Row::ThreadAnchor, Sequence::from_u64(9)))
        );
    }
}
