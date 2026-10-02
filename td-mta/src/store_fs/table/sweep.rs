//! Validate/replay every selected checkpoint table with one reusable record buffer.
use super::super::{active::validate_view, LoadedOverlay, LockedRoot};
use super::{TableInputError, TableReplay, TableReplayError};
use crate::{
    format::{self, bindings::Selection, table::MAX_RECORD_BYTES, Table, TABLE_COUNT},
    ports::{Crypto, ViewIdentity},
};

#[derive(Debug)]
pub enum TableSweepError {
    Input(TableInputError),
    Replay(TableReplayError<format::Error>),
    Format(format::Error),
    ByteLimit,
    RowLimit,
    Failed,
    Incomplete,
}
impl std::fmt::Display for TableSweepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "selected table sweep: {self:?}")
    }
}
impl std::error::Error for TableSweepError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Input(e) => Some(e),
            Self::Replay(e) => Some(e),
            Self::Format(e) => Some(e),
            _ => None,
        }
    }
}
impl From<TableReplayError<format::Error>> for TableSweepError {
    fn from(error: TableReplayError<format::Error>) -> Self {
        match error {
            TableReplayError::Merge(crate::merge::Error::Sink(format::Error::Limit)) => {
                Self::RowLimit
            }
            other => Self::Replay(other),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TableSweepLimits {
    pub bytes: u64,
    pub rows: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableSweepStep {
    Opened(Table),
    Record(Table),
    Exhausted(Table),
    Verified { table: Table, rows: u64 },
    Complete,
}
pub struct TableSweep<'r, 'c, 'm, 't, 'o, 'b, 's, C: Crypto> {
    root: &'r LockedRoot,
    crypto: &'c C,
    selection: Selection<'m>,
    active: &'o LoadedOverlay<'r, 'b, 's>,
    scratch: Option<&'t mut [u8; MAX_RECORD_BYTES]>,
    replay: Option<TableReplay<'r, 'c, 'm, 't, 'o, 'b, 's, C>>,
    drain: bool,
    table: usize,
    max_rows: u64,
    rows: u64,
    bytes: u64,
    counts: [u64; TABLE_COUNT],
    complete: bool,
    failed: bool,
}
impl<'r, 'c, 'm, 't, 'o, 'b, 's, C: Crypto> TableSweep<'r, 'c, 'm, 't, 'o, 'b, 's, C> {
    /// Caller retains actual immutable ownership/barriers and admits each replay step.
    pub fn new(
        root: &'r LockedRoot,
        crypto: &'c C,
        selection: Selection<'m>,
        active: &'o LoadedOverlay<'r, 'b, 's>,
        scratch: &'t mut [u8; MAX_RECORD_BYTES],
        limits: TableSweepLimits,
    ) -> Result<Self, TableSweepError> {
        validate_view(selection, active.identity()).map_err(TableSweepError::Format)?;
        let mut bytes = 0u64;
        for tag in 1..=TABLE_COUNT {
            let table = Table::from_tag(
                u16::try_from(tag).map_err(|_| TableSweepError::Format(format::Error::Overflow))?,
            )
            .map_err(TableSweepError::Format)?;
            let descriptor = selection
                .manifest()
                .table(table)
                .map_err(TableSweepError::Format)?;
            bytes = bytes
                .checked_add(descriptor.file_bytes)
                .ok_or(TableSweepError::Format(format::Error::Overflow))?;
        }
        if bytes > limits.bytes {
            return Err(TableSweepError::ByteLimit);
        }
        Ok(Self {
            root,
            crypto,
            selection,
            active,
            scratch: Some(scratch),
            replay: None,
            drain: false,
            table: 0,
            max_rows: limits.rows,
            rows: 0,
            bytes,
            counts: [0; TABLE_COUNT],
            complete: false,
            failed: false,
        })
    }
    pub const fn is_failed(&self) -> bool {
        self.failed
    }
    pub const fn is_complete(&self) -> bool {
        self.complete && !self.failed
    }
    /// One open, replay advance or selected completion. A replay step may emit
    /// multiple overlay rows; their existing bounded merge-work contract applies.
    pub fn advance(&mut self) -> Result<TableSweepStep, TableSweepError> {
        if self.failed {
            return Err(TableSweepError::Failed);
        }
        self.failed = true;
        let step = self.work()?;
        self.failed = false;
        Ok(step)
    }
    fn work(&mut self) -> Result<TableSweepStep, TableSweepError> {
        if self.complete {
            return Ok(TableSweepStep::Complete);
        }
        if self.table == TABLE_COUNT {
            self.complete = true;
            return Ok(TableSweepStep::Complete);
        }
        let tag = self
            .table
            .checked_add(1)
            .and_then(|n| u16::try_from(n).ok())
            .ok_or(TableSweepError::Format(format::Error::Overflow))?;
        let table = Table::from_tag(tag).map_err(TableSweepError::Format)?;
        if self.drain {
            let replay = self.replay.take().ok_or(TableSweepError::Incomplete)?;
            let rows = &mut self.rows;
            let limit = self.max_rows;
            let (complete, scratch) = replay
                .finish_reuse(|_| count(rows, limit))
                .map_err(TableSweepError::from)?;
            let final_rows = complete.rows();
            *self
                .counts
                .get_mut(self.table)
                .ok_or(TableSweepError::Format(format::Error::Limit))? = final_rows;
            drop(complete);
            self.scratch = Some(scratch);
            self.drain = false;
            self.table = self
                .table
                .checked_add(1)
                .ok_or(TableSweepError::Format(format::Error::Overflow))?;
            return Ok(TableSweepStep::Verified {
                table,
                rows: final_rows,
            });
        }
        if let Some(replay) = self.replay.as_mut() {
            let rows = &mut self.rows;
            let limit = self.max_rows;
            if replay
                .advance(|_| count(rows, limit))
                .map_err(TableSweepError::from)?
            {
                return Ok(TableSweepStep::Record(table));
            }
            self.drain = true;
            return Ok(TableSweepStep::Exhausted(table));
        }
        let scratch = self.scratch.take().ok_or(TableSweepError::Incomplete)?;
        let descriptor = self
            .selection
            .manifest()
            .table(table)
            .map_err(TableSweepError::Format)?;
        self.replay = Some(
            self.root
                .open_table(
                    self.crypto,
                    self.selection,
                    table,
                    descriptor.file_bytes,
                    scratch,
                )
                .map_err(TableSweepError::Input)?
                .into_replay(self.active)
                .map_err(TableSweepError::Input)?,
        );
        Ok(TableSweepStep::Opened(table))
    }
    pub fn finish(
        self,
    ) -> Result<(CompleteTables, &'t mut [u8; MAX_RECORD_BYTES]), TableSweepError> {
        if self.failed {
            return Err(TableSweepError::Failed);
        }
        if !self.complete {
            return Err(TableSweepError::Incomplete);
        }
        let scratch = self.scratch.ok_or(TableSweepError::Incomplete)?;
        Ok((
            CompleteTables {
                identity: self.active.identity(),
                rows: self.rows,
                table_bytes: self.bytes,
                counts: self.counts,
            },
            scratch,
        ))
    }
}
fn count(rows: &mut u64, limit: u64) -> Result<(), format::Error> {
    if *rows >= limit {
        return Err(format::Error::Limit);
    }
    *rows = rows.checked_add(1).ok_or(format::Error::Overflow)?;
    Ok(())
}
/// All selected checkpoint files and their final replay rows; history, references and pins remain separate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompleteTables {
    identity: ViewIdentity,
    rows: u64,
    table_bytes: u64,
    counts: [u64; TABLE_COUNT],
}
impl CompleteTables {
    pub const fn identity(self) -> ViewIdentity {
        self.identity
    }
    pub const fn rows(self) -> u64 {
        self.rows
    }
    pub const fn table_bytes(self) -> u64 {
        self.table_bytes
    }
    pub const fn counts(&self) -> &[u64; TABLE_COUNT] {
        &self.counts
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) fn probe(
    root: &LockedRoot,
    tables: &super::ProbeBytes,
    active: &super::super::active::ProbeBytes,
    frames: &mut [u8],
    cells: &mut [crate::overlay::Cell],
    scratch: &mut [u8; MAX_RECORD_BYTES],
) {
    use td_crypto::Provider;
    let original = scratch.as_ptr();
    for (through, end, expected) in [(1, 96, 1), (2, 256, 0)] {
        let loaded = root
            .load_active_overlay(
                &Provider,
                active.selection(),
                active.view(through, end),
                end,
                frames,
                cells,
            )
            .unwrap();
        let mut sweep = TableSweep::new(
            root,
            &Provider,
            tables.selection(),
            &loaded,
            scratch,
            TableSweepLimits {
                bytes: 1345,
                rows: expected,
            },
        )
        .unwrap();
        let mut opened = 0;
        let mut verified = 0;
        let mut records = 0;
        for _ in 0..40 {
            match sweep.advance().unwrap() {
                TableSweepStep::Opened(table) => {
                    opened += 1;
                    assert_eq!(table.tag() as usize, opened);
                }
                TableSweepStep::Record(_) => records += 1,
                TableSweepStep::Verified { table, rows } => {
                    verified += 1;
                    assert_eq!(table.tag() as usize, verified);
                    assert_eq!(rows, if table == Table::Blobs { expected } else { 0 });
                }
                TableSweepStep::Exhausted(_) => {}
                TableSweepStep::Complete => break,
            }
        }
        assert!(sweep.is_complete());
        assert_eq!((opened, verified, records), (TABLE_COUNT, TABLE_COUNT, 1));
        assert_eq!(sweep.advance().unwrap(), TableSweepStep::Complete);
        let (complete, scratch) = sweep.finish().unwrap();
        assert_eq!(scratch.as_ptr(), original);
        assert_eq!(complete.identity(), active.view(through, end));
        assert_eq!(complete.rows(), expected);
        assert_eq!(complete.table_bytes(), 1345);
        assert_eq!(complete.counts().first(), Some(&expected));
        assert!(complete.counts().iter().skip(1).all(|&rows| rows == 0));
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::super::super::{active, tests::Fixture};
    use super::super::fixture::{self, Bytes, ACCOUNT};
    use super::*;
    use crate::{
        overlay::Cell,
        store_paths::{AccountEntry, Number},
    };
    use td_crypto::Provider;
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
    #[test]
    fn all_selected_tables_replay_and_reclaim_one_record_buffer() {
        let fixture = Fixture::new();
        let (root, tables, active) = setup(&fixture);
        probe(
            &root,
            &tables,
            &active,
            &mut [0; 256],
            &mut [Cell::EMPTY; 8],
            &mut [0; MAX_RECORD_BYTES],
        );
        assert!(std::mem::size_of::<TableSweep<'_, '_, '_, '_, '_, '_, '_, Provider>>() <= 4096);
    }
    #[test]
    fn total_bytes_rows_and_incomplete_state_refuse() {
        let fixture = Fixture::new();
        let (root, tables, active) = setup(&fixture);
        let mut frames = [0; 256];
        let mut cells = [Cell::EMPTY; 8];
        let mut scratch = [0; MAX_RECORD_BYTES];
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
        assert!(matches!(
            TableSweep::new(
                &root,
                &Provider,
                tables.selection(),
                &loaded,
                &mut scratch,
                TableSweepLimits {
                    bytes: 1344,
                    rows: 1
                }
            ),
            Err(TableSweepError::ByteLimit)
        ));
        let sweep = TableSweep::new(
            &root,
            &Provider,
            tables.selection(),
            &loaded,
            &mut scratch,
            TableSweepLimits {
                bytes: 1345,
                rows: 1,
            },
        )
        .unwrap();
        assert!(matches!(sweep.finish(), Err(TableSweepError::Incomplete)));
        let mut sweep = TableSweep::new(
            &root,
            &Provider,
            tables.selection(),
            &loaded,
            &mut scratch,
            TableSweepLimits {
                bytes: 1345,
                rows: 0,
            },
        )
        .unwrap();
        assert_eq!(
            sweep.advance().unwrap(),
            TableSweepStep::Opened(Table::Blobs)
        );
        assert!(matches!(sweep.advance(), Err(TableSweepError::RowLimit)));
        assert!(sweep.is_failed());
        assert!(matches!(sweep.advance(), Err(TableSweepError::Failed)));
        assert!(matches!(sweep.finish(), Err(TableSweepError::Failed)));
        let mut sweep = TableSweep::new(
            &root,
            &Provider,
            tables.selection(),
            &loaded,
            &mut scratch,
            TableSweepLimits {
                bytes: 1345,
                rows: 1,
            },
        )
        .unwrap();
        for _ in 0..40 {
            if matches!(
                sweep.advance().unwrap(),
                TableSweepStep::Verified {
                    table: Table::Imports,
                    ..
                }
            ) {
                break;
            }
        }
        assert!(!sweep.is_complete());
        assert!(matches!(sweep.finish(), Err(TableSweepError::Incomplete)));
        // Same active snapshot with a different selected generation: constructor must refuse.
        use crate::{
            format::{container::Current, manifest},
            ports::Digest,
        };
        let selected = tables.selection();
        let metadata = selected.manifest();
        let descriptors = std::array::from_fn::<_, TABLE_COUNT, _>(|i| {
            metadata
                .table(Table::from_tag(i as u16 + 1).unwrap())
                .unwrap()
        });
        let history: Vec<_> = (0..metadata.history_count())
            .map(|i| metadata.history(i).unwrap())
            .collect();
        let mut manifest_bytes = vec![0; format::MAX_MANIFEST_BYTES];
        let n = manifest::encode(
            &Provider,
            manifest::Header {
                generation: 3,
                ..metadata.header()
            },
            &descriptors,
            &history,
            &mut manifest_bytes,
        )
        .unwrap();
        manifest_bytes.truncate(n);
        let mut hash = Provider.sha256().unwrap();
        hash.update(&manifest_bytes).unwrap();
        let mut current_bytes = tables.current.clone();
        Current {
            generation: 3,
            manifest_digest: hash.finish().unwrap(),
            ..selected.current()
        }
        .encode(&Provider, &mut current_bytes)
        .unwrap();
        let mut format_bytes = vec![0; format::FORMAT_BYTES];
        selected
            .store()
            .encode(&Provider, &mut format_bytes)
            .unwrap();
        let mismatched = Selection::decode(
            &Provider,
            ACCOUNT,
            &format_bytes,
            &current_bytes,
            &manifest_bytes,
        )
        .unwrap();
        assert!(matches!(
            TableSweep::new(
                &root,
                &Provider,
                mismatched,
                &loaded,
                &mut scratch,
                TableSweepLimits {
                    bytes: 1345,
                    rows: 1
                }
            ),
            Err(TableSweepError::Format(format::Error::InvalidValue))
        ));
    }
    #[test]
    fn residual_rows_share_admission_with_earlier_tables() {
        use crate::format::{
            frame, operation::Operation, Sequence, FRAME_FOOTER_BYTES, FRAME_HEADER_BYTES,
            JOURNAL_HEADER_BYTES,
        };
        let fixture = Fixture::new();
        let (root, tables, active) = setup(&fixture);
        let mut journal = active::fixture::journal();
        journal.truncate(JOURNAL_HEADER_BYTES);
        let mut frame = vec![0; FRAME_HEADER_BYTES + 28 + FRAME_FOOTER_BYTES];
        Operation::put(Table::Threads, &[0x55; 16], &[])
            .unwrap()
            .encode(&mut frame[FRAME_HEADER_BYTES..FRAME_HEADER_BYTES + 28])
            .unwrap();
        frame::seal(&Provider, Sequence::from_u64(2), 1, &mut frame).unwrap();
        journal.extend_from_slice(&frame);
        active::fixture::write(&root, &journal);
        let mut frames = [0; 256];
        let mut cells = [Cell::EMPTY; 8];
        let mut scratch = [0; MAX_RECORD_BYTES];
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
        for limit in [1, 2] {
            let mut sweep = TableSweep::new(
                &root,
                &Provider,
                tables.selection(),
                &loaded,
                &mut scratch,
                TableSweepLimits {
                    bytes: 1345,
                    rows: limit,
                },
            )
            .unwrap();
            let mut verified = 0;
            let mut exhausted = None;
            let mut refused = false;
            for _ in 0..40 {
                match sweep.advance() {
                    Ok(TableSweepStep::Exhausted(table)) => exhausted = Some(table),
                    Ok(TableSweepStep::Verified { .. }) => verified += 1,
                    Ok(TableSweepStep::Complete) => break,
                    Err(TableSweepError::RowLimit) => {
                        refused = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => panic!("unexpected error: {e}"),
                }
            }
            if limit == 1 {
                assert!(refused);
                assert_eq!(verified, 5);
                assert_eq!(exhausted, Some(Table::Threads));
                assert!(matches!(sweep.finish(), Err(TableSweepError::Failed)));
            } else {
                let (done, _) = sweep.finish().unwrap();
                assert_eq!(done.rows(), 2);
                assert_eq!(done.counts().first(), Some(&1));
                assert_eq!(done.counts().get(5), Some(&1));
                assert_eq!(done.counts().iter().sum::<u64>(), 2);
            }
        }
    }
    #[test]
    fn missing_last_table_cannot_complete_or_return_scratch_evidence() {
        let fixture = Fixture::new();
        let (root, tables, active) = setup(&fixture);
        std::fs::remove_file(
            fixture
                .path
                .join(fixture::name(Table::Imports).as_path().unwrap()),
        )
        .unwrap();
        let mut frames = [0; 256];
        let mut cells = [Cell::EMPTY; 8];
        let mut scratch = [0; MAX_RECORD_BYTES];
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
        let mut sweep = TableSweep::new(
            &root,
            &Provider,
            tables.selection(),
            &loaded,
            &mut scratch,
            TableSweepLimits {
                bytes: 1345,
                rows: 1,
            },
        )
        .unwrap();
        let mut verified = 0;
        let mut failed = false;
        for _ in 0..40 {
            match sweep.advance() {
                Ok(TableSweepStep::Verified { .. }) => verified += 1,
                Err(TableSweepError::Input(TableInputError::Io(e))) => {
                    assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
                    failed = true;
                    break;
                }
                Ok(_) => {}
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert!(failed);
        assert_eq!(verified, TABLE_COUNT - 1);
        assert!(matches!(sweep.finish(), Err(TableSweepError::Failed)));
    }
    #[test]
    fn late_extent_change_refuses_selected_completion() {
        use std::io::Write;
        let fixture = Fixture::new();
        let (root, tables, active) = setup(&fixture);
        let mut frames = [0; 256];
        let mut cells = [Cell::EMPTY; 8];
        let mut scratch = [0; MAX_RECORD_BYTES];
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
        let mut sweep = TableSweep::new(
            &root,
            &Provider,
            tables.selection(),
            &loaded,
            &mut scratch,
            TableSweepLimits {
                bytes: 1345,
                rows: 1,
            },
        )
        .unwrap();
        for _ in 0..3 {
            sweep.advance().unwrap();
        }
        let path = fixture
            .path
            .join(fixture::name(Table::Blobs).as_path().unwrap());
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(&[0])
            .unwrap();
        assert!(matches!(
            sweep.advance(),
            Err(TableSweepError::Replay(TableReplayError::Input(_)))
        ));
        assert!(matches!(sweep.finish(), Err(TableSweepError::Failed)));
    }
}
