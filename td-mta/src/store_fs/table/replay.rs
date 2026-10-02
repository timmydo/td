//! Table input and provisional overlay replay share one failure boundary.
use super::super::{active::validate_view, LoadedOverlay};
use super::{CompleteTable, TableInput, TableInputError};
use crate::{
    merge::{self, Merge},
    ports::{Crypto, Record, ViewIdentity},
};

#[derive(Debug)]
pub enum Error<E> {
    Input(TableInputError),
    Merge(merge::Error<E>),
    Failed,
}
impl<E: std::fmt::Display> std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Input(e) => write!(f, "table replay input: {e}"),
            Self::Merge(e) => e.fmt(f),
            Self::Failed => f.write_str("table replay already failed"),
        }
    }
}
impl<E: std::error::Error + 'static> std::error::Error for Error<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Input(e) => Some(e),
            Self::Merge(e) => Some(e),
            Self::Failed => None,
        }
    }
}
pub struct TableReplay<'r, 'c, 'm, 't, 'o, 'b, 's, C: Crypto> {
    input: TableInput<'r, 'c, 'm, 't, C>,
    merge: Merge<'o, 'b, 's>,
    active: &'o LoadedOverlay<'r, 'b, 's>,
    failed: bool,
}
impl<'r, 'c, 'm, 't, C: Crypto> TableInput<'r, 'c, 'm, 't, C> {
    /// Bind the loaded active identity to this selected table before replay.
    /// The caller retains actual pins and admits input plus merge/sink work.
    pub fn into_replay<'o, 'b, 's>(
        self,
        active: &'o LoadedOverlay<'r, 'b, 's>,
    ) -> Result<TableReplay<'r, 'c, 'm, 't, 'o, 'b, 's, C>, TableInputError> {
        if self.failed {
            return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe).into());
        }
        if self.remaining != self.verifier.header().record_count {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput).into());
        }
        validate_view(self.selection, active.identity())?;
        let merge = Merge::new(self.verifier.header(), active.overlay())?;
        Ok(TableReplay {
            input: self,
            merge,
            active,
            failed: false,
        })
    }
}
impl<'r, 't, 'o, 'b, 's, C: Crypto> TableReplay<'r, '_, '_, 't, 'o, 'b, 's, C> {
    pub const fn is_failed(&self) -> bool {
        self.failed
    }
    /// One checkpoint record plus bounded intervening overlay work. False
    /// means declared table exhaustion only; finish still must establish EOF.
    pub fn advance<E>(
        &mut self,
        sink: impl FnMut(Record<'_>) -> Result<(), E>,
    ) -> Result<bool, Error<E>> {
        if self.failed {
            return Err(Error::Failed);
        }
        self.failed = true;
        let next = self.input.next_record().map_err(Error::Input)?;
        let advanced = if let Some(record) = next {
            self.merge.push(record, sink).map_err(Error::Merge)?;
            true
        } else {
            false
        };
        self.failed = false;
        Ok(advanced)
    }
    /// Verify the complete selected table before emitting residual overlay rows.
    /// Earlier callback effects remain provisional on every failure.
    pub fn finish<E>(
        self,
        sink: impl FnMut(Record<'_>) -> Result<(), E>,
    ) -> Result<CompleteReplay<'r, 'o, 'b, 's>, Error<E>> {
        self.finish_reuse(sink).map(|(complete, _)| complete)
    }
    /// Reclaim record scratch after selected completion and residual sink work.
    pub fn finish_reuse<E>(
        self,
        sink: impl FnMut(Record<'_>) -> Result<(), E>,
    ) -> Result<
        (
            CompleteReplay<'r, 'o, 'b, 's>,
            &'t mut [u8; crate::format::table::MAX_RECORD_BYTES],
        ),
        Error<E>,
    > {
        if self.failed {
            return Err(Error::Failed);
        }
        let (table, scratch) = self.input.finish_reuse().map_err(Error::Input)?;
        let rows = self.merge.finish(sink).map_err(Error::Merge)?;
        Ok((
            CompleteReplay {
                table,
                active: self.active,
                rows,
            },
            scratch,
        ))
    }
}
/// Selected table/prefix completion only. Final references, pins and any
/// staged output's durability/publication remain coordinator responsibilities.
pub struct CompleteReplay<'r, 'o, 'b, 's> {
    table: CompleteTable<'r>,
    active: &'o LoadedOverlay<'r, 'b, 's>,
    rows: u64,
}
impl CompleteReplay<'_, '_, '_, '_> {
    pub fn table(&self) -> &CompleteTable<'_> {
        &self.table
    }
    pub fn identity(&self) -> ViewIdentity {
        self.active.identity()
    }
    pub const fn rows(&self) -> u64 {
        self.rows
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
) {
    use crate::format::Table;
    for (through, end, expected) in [(1, 96, 1), (2, 256, 0)] {
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
        let mut replay = root
            .open_table(
                &td_crypto::Provider,
                table.selection(),
                Table::Blobs,
                225,
                scratch,
            )
            .unwrap()
            .into_replay(&loaded)
            .unwrap();
        let mut rows = 0;
        assert!(replay
            .advance(|_| {
                rows += 1;
                Ok::<_, ()>(())
            })
            .unwrap());
        assert!(!replay
            .advance(|_| {
                rows += 1;
                Ok::<_, ()>(())
            })
            .unwrap());
        let complete = replay
            .finish(|_| {
                rows += 1;
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(complete.rows(), expected);
        assert_eq!(rows, expected);
        assert_eq!(complete.identity(), active.view(through, end));
        assert_eq!(complete.table().file().len(), 225);
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
    let replay = root
        .open_table(
            &td_crypto::Provider,
            table.selection(),
            Table::Blobs,
            225,
            scratch,
        )
        .unwrap()
        .into_replay(&loaded)
        .unwrap();
    assert!(matches!(
        replay.finish(|_| Ok::<_, ()>(())),
        Err(Error::Input(_))
    ));
    let mut replay = root
        .open_table(
            &td_crypto::Provider,
            table.selection(),
            Table::Blobs,
            225,
            scratch,
        )
        .unwrap()
        .into_replay(&loaded)
        .unwrap();
    assert!(matches!(
        replay.advance(|_| Err(7)),
        Err(Error::Merge(merge::Error::Sink(7)))
    ));
    assert!(replay.is_failed());
    assert!(matches!(
        replay.advance(|_| Ok::<_, i32>(())),
        Err(Error::Failed)
    ));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::super::{active, tests::Fixture, LockedRoot};
    use super::super::{
        fixture::{self, Bytes, ACCOUNT},
        MAX_RECORD_BYTES,
    };
    use super::*;
    use crate::{
        format::{self, container::Current, manifest, Sequence, Table, TABLE_HEADER_BYTES},
        overlay::Cell,
        store_paths::{AccountEntry, Number},
    };
    use td_crypto::{Digest, Provider};
    fn setup(fixture: &Fixture) -> (LockedRoot, Bytes, active::ProbeBytes) {
        let root = fixture.locked();
        root.create_accounts_directory().unwrap();
        for entry in [
            AccountEntry::Root,
            AccountEntry::Metadata,
            AccountEntry::Checkpoints,
            AccountEntry::Checkpoint(Number::new(2).unwrap()),
            AccountEntry::Journals,
        ] {
            root.create_account_directory(ACCOUNT, entry).unwrap();
        }
        let table = fixture::prepare(&root);
        let active = active::prepare_probe(&root);
        (root, table, active)
    }
    fn original() -> Vec<u8> {
        fixture::hex(fixture::TABLES.first().unwrap())
    }
    fn digest(bytes: &[u8]) -> [u8; 32] {
        let mut hash = Provider.sha256().unwrap();
        hash.update(bytes).unwrap();
        hash.finish().unwrap()
    }
    fn generation(bytes: &mut Bytes, next: u64) {
        let selected = bytes.selection();
        let manifest = selected.manifest();
        let tables: [manifest::TableDescriptor; format::TABLE_COUNT] = std::array::from_fn(|i| {
            manifest
                .table(Table::from_tag((i + 1) as u16).unwrap())
                .unwrap()
        });
        let history: Vec<_> = (0..manifest.history_count())
            .map(|i| manifest.history(i).unwrap())
            .collect();
        let mut encoded = vec![0; format::MAX_MANIFEST_BYTES];
        let length = manifest::encode(
            &Provider,
            manifest::Header {
                generation: next,
                ..manifest.header()
            },
            &tables,
            &history,
            &mut encoded,
        )
        .unwrap();
        encoded.truncate(length);
        bytes.manifest = encoded;
        let current = Current {
            generation: next,
            manifest_digest: digest(&bytes.manifest),
            ..Current::decode(&Provider, &bytes.current).unwrap()
        };
        current.encode(&Provider, &mut bytes.current).unwrap();
    }
    #[test]
    fn selected_table_replay_retains_verified_files_and_counts_live_rows() {
        let fixture = Fixture::new();
        let (root, table, active) = setup(&fixture);
        let mut frames = [0; 160];
        let mut cells = [Cell::EMPTY; 2];
        let mut scratch = vec![0; MAX_RECORD_BYTES];
        probe(
            &root,
            &table,
            &active,
            &mut frames,
            &mut cells,
            scratch.as_mut_slice().try_into().unwrap(),
        );
        let loaded = root
            .load_active_overlay(
                &Provider,
                active.selection(),
                active.view(2, 256),
                256,
                &mut frames,
                &mut cells,
            )
            .unwrap();
        for tag in 2..=11 {
            let input = root
                .open_table(
                    &Provider,
                    table.selection(),
                    Table::from_tag(tag).unwrap(),
                    225,
                    scratch.as_mut_slice().try_into().unwrap(),
                )
                .unwrap();
            let mut replay = input.into_replay(&loaded).unwrap();
            assert!(!replay.advance(|_| Ok::<_, ()>(())).unwrap());
            assert_eq!(replay.finish(|_| Ok::<_, ()>(())).unwrap().rows(), 0);
        }
    }
    #[test]
    fn mismatched_selection_and_preconsumed_or_failed_input_cannot_start_replay() {
        let fixture = Fixture::new();
        let (root, mut table, active) = setup(&fixture);
        let mut frames = [0; 160];
        let mut cells = [Cell::EMPTY; 2];
        let mut scratch = vec![0; MAX_RECORD_BYTES];
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
        let mut input = root
            .open_table(
                &Provider,
                table.selection(),
                Table::Blobs,
                225,
                scratch.as_mut_slice().try_into().unwrap(),
            )
            .unwrap();
        assert!(input.next_record().unwrap().is_some());
        assert!(
            matches!(input.into_replay(&loaded),Err(TableInputError::Io(e)) if e.kind()==std::io::ErrorKind::InvalidInput)
        );
        let mut input = root
            .open_table(
                &Provider,
                table.selection(),
                Table::Blobs,
                225,
                scratch.as_mut_slice().try_into().unwrap(),
            )
            .unwrap();
        fixture::write(&root, Table::Blobs, &[]);
        assert!(input.next_record().is_err());
        assert!(
            matches!(input.into_replay(&loaded),Err(TableInputError::Io(e)) if e.kind()==std::io::ErrorKind::BrokenPipe)
        );
        fixture::write(&root, Table::Blobs, &original());
        drop(loaded);
        generation(&mut table, 3);
        let mut view = active.view(1, 96);
        view.generation = 3;
        let loaded = root
            .load_active_overlay(
                &Provider,
                table.selection(),
                view,
                96,
                &mut frames,
                &mut cells,
            )
            .unwrap();
        let input = root
            .open_table(
                &Provider,
                active.selection(),
                Table::Blobs,
                225,
                scratch.as_mut_slice().try_into().unwrap(),
            )
            .unwrap();
        assert!(matches!(
            input.into_replay(&loaded),
            Err(TableInputError::Container(
                format::container::Error::Format(format::Error::InvalidValue)
            ))
        ));
    }
    #[test]
    fn input_errors_retire_replay_and_shrink_after_output_refuses_completion() {
        let fixture = Fixture::new();
        let (root, table, active) = setup(&fixture);
        let mut frames = [0; 160];
        let mut cells = [Cell::EMPTY; 2];
        let mut scratch = vec![0; MAX_RECORD_BYTES];
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
        let mut replay = root
            .open_table(
                &Provider,
                table.selection(),
                Table::Blobs,
                225,
                scratch.as_mut_slice().try_into().unwrap(),
            )
            .unwrap()
            .into_replay(&loaded)
            .unwrap();
        let mut corrupt = original();
        *corrupt.last_mut().unwrap() ^= 1;
        fixture::write(&root, Table::Blobs, &corrupt);
        assert!(matches!(
            replay.advance(|_| Ok::<_, ()>(())),
            Err(Error::Input(_))
        ));
        assert!(replay.is_failed());
        assert!(matches!(
            replay.advance(|_| Ok::<_, ()>(())),
            Err(Error::Failed)
        ));
        assert!(matches!(
            replay.finish(|_| Ok::<_, ()>(())),
            Err(Error::Failed)
        ));
        fixture::write(&root, Table::Blobs, &original());
        let mut replay = root
            .open_table(
                &Provider,
                table.selection(),
                Table::Blobs,
                225,
                scratch.as_mut_slice().try_into().unwrap(),
            )
            .unwrap()
            .into_replay(&loaded)
            .unwrap();
        let mut rows = 0;
        assert!(replay
            .advance(|_| {
                rows += 1;
                Ok::<_, ()>(())
            })
            .unwrap());
        assert_eq!(rows, 1);
        fixture::write(&root, Table::Blobs, &[]);
        assert!(matches!(
            replay.finish(|_| Ok::<_, ()>(())),
            Err(Error::Input(_))
        ));
    }
    #[test]
    fn final_table_digest_is_checked_before_residual_overlay_output() {
        use crate::format::{frame, operation::Operation, FRAME_FOOTER_BYTES, FRAME_HEADER_BYTES};
        let fixture = Fixture::new();
        let (root, table, active) = setup(&fixture);
        let original = original();
        let mut changed = original.clone();
        // Change a digest byte in the Blob row, then reseal its record only.
        *changed
            .get_mut(TABLE_HEADER_BYTES + 16 + 16 + 1 + 8)
            .unwrap() ^= 1;
        let end = changed.len() - 32;
        let sum = digest(changed.get(TABLE_HEADER_BYTES..end).unwrap());
        changed.get_mut(end..).unwrap().copy_from_slice(&sum);
        fixture::write(&root, Table::Blobs, &changed);
        let value = original
            .get(TABLE_HEADER_BYTES + 16 + 16..original.len() - 32)
            .unwrap();
        let operations = [
            Operation::put(Table::Blobs, &[0x33; 16], value).unwrap(),
            Operation::put(Table::Blobs, &[0x55; 16], value).unwrap(),
        ];
        let mut encoded = vec![
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
            offset += op.encode(encoded.get_mut(offset..).unwrap()).unwrap();
        }
        frame::seal(&Provider, Sequence::from_u64(2), 2, &mut encoded).unwrap();
        let mut journal = active::fixture::journal().get(..96).unwrap().to_vec();
        journal.extend_from_slice(&encoded);
        active::fixture::write(&root, &journal);
        let mut frames = vec![0; encoded.len()];
        let mut cells = [Cell::EMPTY; 2];
        let mut scratch = vec![0; MAX_RECORD_BYTES];
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
        let mut replay = root
            .open_table(
                &Provider,
                table.selection(),
                Table::Blobs,
                225,
                scratch.as_mut_slice().try_into().unwrap(),
            )
            .unwrap()
            .into_replay(&loaded)
            .unwrap();
        let mut rows = 0;
        assert!(replay
            .advance(|_| {
                rows += 1;
                Ok::<_, ()>(())
            })
            .unwrap());
        assert_eq!(rows, 2);
        assert!(matches!(
            replay.finish(|_| {
                rows += 1;
                Ok::<_, ()>(())
            }),
            Err(Error::Input(TableInputError::Container(
                format::container::Error::Checksum
            )))
        ));
        assert_eq!(rows, 2);
        fixture::write(&root, Table::Blobs, &original);
        let mut replay = root
            .open_table(
                &Provider,
                table.selection(),
                Table::Blobs,
                225,
                scratch.as_mut_slice().try_into().unwrap(),
            )
            .unwrap()
            .into_replay(&loaded)
            .unwrap();
        fn collect(row: Record<'_>, seen: &mut Vec<crate::ids::BlobId>) -> Result<(), ()> {
            let crate::format::key::Key::Blob(id) = row.key else {
                return Err(());
            };
            seen.push(id);
            Ok(())
        }
        let mut seen = Vec::new();
        assert!(replay.advance(|row| collect(row, &mut seen)).unwrap());
        assert_eq!(
            seen,
            vec![
                crate::ids::BlobId::from_bytes([0x33; 16]),
                crate::ids::BlobId::from_bytes([0x44; 16])
            ]
        );
        let complete = replay.finish(|row| collect(row, &mut seen)).unwrap();
        assert_eq!(complete.rows(), 3);
        assert_eq!(
            seen,
            vec![
                crate::ids::BlobId::from_bytes([0x33; 16]),
                crate::ids::BlobId::from_bytes([0x44; 16]),
                crate::ids::BlobId::from_bytes([0x55; 16])
            ]
        );
    }
}
