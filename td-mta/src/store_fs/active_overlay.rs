//! Captured active prefix loaded into caller-owned replay storage.
pub use super::journal_input::Error as OverlayInputError;
use super::{
    active::validate_view, input::fill_exact_using, CompletePrefix, LockedRoot, PrefixReader,
};
use crate::{
    format::{
        bindings::Selection, container::Error as ContainerError,
        journal_stream::Error as StreamError, Error as FormatError, JOURNAL_HEADER_BYTES,
        MAX_JOURNAL_FRAME_BYTES, MAX_JOURNAL_OPERATIONS,
    },
    overlay::{Cell, Overlay},
    ports::{Crypto, ViewIdentity},
    store_paths::Number,
};
use std::io;

const MAX_READ_CALLS: usize = 128;

/// Owns the consumed prefix descriptor and borrows its checked replay storage.
/// Actual pins, history-floor checks and complete final-row validation are external.
pub struct LoadedOverlay<'r, 'b, 's> {
    file: CompletePrefix<'r>,
    overlay: Overlay<'b, 's>,
    identity: ViewIdentity,
}
impl<'r, 'b, 's> LoadedOverlay<'r, 'b, 's> {
    pub fn file(&self) -> &CompletePrefix<'r> {
        &self.file
    }
    pub fn overlay(&self) -> &Overlay<'b, 's> {
        &self.overlay
    }
    pub const fn identity(&self) -> ViewIdentity {
        self.identity
    }
}
impl LockedRoot {
    /// Caller holds real view ownership/barriers and admits the entire bounded
    /// read/hash/sort work unit. Buffers may be overwritten on failure.
    pub fn load_active_overlay<'r, 'b, 's>(
        &'r self,
        crypto: &impl Crypto,
        selection: Selection<'_>,
        view: ViewIdentity,
        max_bytes: u64,
        frames: &'b mut [u8],
        cells: &'s mut [Cell],
    ) -> Result<LoadedOverlay<'r, 'b, 's>, OverlayInputError> {
        validate_view(selection, view)?;
        let minimum_cells = view
            .committed_sequence
            .number()
            .checked_sub(view.checkpoint.number())
            .and_then(|v| usize::try_from(v).ok())
            .ok_or(FormatError::Overflow)?;
        if view.committed_offset > max_bytes
            || cells.len() < minimum_cells
            || frames.len() > MAX_JOURNAL_FRAME_BYTES
            || cells.len() > MAX_JOURNAL_OPERATIONS
        {
            return Err(io::Error::from(io::ErrorKind::InvalidInput).into());
        }
        let count = view
            .committed_offset
            .checked_sub(JOURNAL_HEADER_BYTES as u64)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or(FormatError::Overflow)?;
        let frames = frames
            .get_mut(..count)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let selected = selection.manifest().header();
        let file = self.open_journal_prefix(
            selected.account,
            Number::new(selected.active_segment).map_err(|_| FormatError::InvalidValue)?,
            view.committed_offset,
            (JOURNAL_HEADER_BYTES + MAX_JOURNAL_FRAME_BYTES) as u64,
        )?;
        load_using(
            crypto,
            selection,
            view,
            file,
            frames,
            cells,
            PrefixReader::read,
        )
    }
}
fn load_using<'r, 'b, 's>(
    crypto: &impl Crypto,
    selection: Selection<'_>,
    view: ViewIdentity,
    mut file: PrefixReader<'r>,
    frames: &'b mut [u8],
    cells: &'s mut [Cell],
    mut read: impl FnMut(&mut PrefixReader<'r>, &mut [u8]) -> io::Result<usize>,
) -> Result<LoadedOverlay<'r, 'b, 's>, OverlayInputError> {
    let mut header = [0; JOURNAL_HEADER_BYTES];
    let mut attempts = MAX_READ_CALLS;
    fill_exact_using(&mut file, &mut header, &mut attempts, &mut read)?;
    selection.check_active_header(crypto, &header)?;
    fill_exact_using(&mut file, frames, &mut attempts, &mut read)?;
    let file = file.finish()?;
    let overlay = Overlay::decode(crypto, &header, frames, cells).map_err(|error| match error {
        StreamError::Journal(ContainerError::Format(FormatError::OutputFull)) => {
            OverlayInputError::Io(io::ErrorKind::InvalidInput.into())
        }
        _ => error.into(),
    })?;
    selection.check_active_prefix(
        view.committed_sequence,
        view.committed_offset,
        overlay.summary(),
    )?;
    Ok(LoadedOverlay {
        file,
        overlay,
        identity: view,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(super) fn probe(
    root: &LockedRoot,
    bytes: &super::active::ProbeBytes,
    frames: &mut [u8],
    cells: &mut [Cell],
) {
    use crate::{
        format::{key::Key, Table},
        ids::BlobId,
        ports::Mutation,
    };
    let empty = root
        .load_active_overlay(
            &td_crypto::Provider,
            bytes.selection(),
            bytes.view(1, 96),
            96,
            frames,
            cells,
        )
        .unwrap();
    assert_eq!(empty.overlay().operation_count(), 0);
    assert_eq!(empty.file().len(), 96);
    drop(empty);
    let loaded = root
        .load_active_overlay(
            &td_crypto::Provider,
            bytes.selection(),
            bytes.view(2, 256),
            256,
            frames,
            cells,
        )
        .unwrap();
    assert_eq!(loaded.identity(), bytes.view(2, 256));
    assert_eq!(loaded.file().len(), 256);
    assert_eq!(loaded.overlay().operation_count(), 2);
    let key = Key::Blob(BlobId::from_bytes([0x44; 16]));
    assert!(
        matches!(loaded.overlay().get(key).unwrap().unwrap().mutation, Mutation::Delete(k) if k == key)
    );
    assert!(loaded
        .overlay()
        .next(Table::Mailboxes, None)
        .unwrap()
        .is_none());
    drop(loaded);
    assert!(root
        .load_active_overlay(
            &td_crypto::Provider,
            bytes.selection(),
            bytes.view(2, 256),
            255,
            frames,
            cells
        )
        .is_err());
    assert!(root
        .load_active_overlay(
            &td_crypto::Provider,
            bytes.selection(),
            bytes.view(2, 256),
            256,
            frames,
            &mut []
        )
        .is_err());
    assert!(
        matches!(root.load_active_overlay(&td_crypto::Provider, bytes.selection(), bytes.view(2,256),256,frames,cells.get_mut(..1).unwrap()),
        Err(OverlayInputError::Io(e)) if e.kind()==io::ErrorKind::InvalidInput)
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::{active::fixture::*, tests::Fixture};
    use super::*;
    use crate::{
        format::{
            container::JournalHeader, frame::DecodeError, journal_stream::Error as StreamError,
            key::Key, Sequence,
        },
        ids::{AccountId, BlobId},
        ports::Mutation,
        store_paths::{AccountEntry, Name},
    };
    use td_crypto::Provider;
    const MAX_BYTES: u64 = (JOURNAL_HEADER_BYTES + MAX_JOURNAL_FRAME_BYTES) as u64;
    fn setup(fixture: &Fixture) -> (LockedRoot, Bytes) {
        let root = fixture.locked();
        root.create_accounts_directory().unwrap();
        for entry in [
            AccountEntry::Root,
            AccountEntry::Metadata,
            AccountEntry::Journals,
        ] {
            root.create_account_directory(ACCOUNT, entry).unwrap();
        }
        let bytes = prepare(&root);
        (root, bytes)
    }
    fn input(root: &LockedRoot, count: u64) -> PrefixReader<'_> {
        root.open_journal_prefix(ACCOUNT, Number::new(2).unwrap(), count, MAX_BYTES)
            .unwrap()
    }
    #[test]
    fn captured_prefix_replays_rows_and_ignores_later_appends() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut frames = [0; 320];
        let mut cells = [Cell::EMPTY; 4];
        probe(&root, &bytes, &mut frames, &mut cells);
        write(&root, &journal());
        let mut calls = 0;
        let loaded = load_using(
            &Provider,
            bytes.selection(),
            bytes.view(2, 256),
            input(&root, 256),
            frames.get_mut(..160).unwrap(),
            &mut cells,
            |file, output| {
                calls += 1;
                let actual = file.read(output)?;
                if calls == 2 {
                    append(&root, b"later incomplete append");
                }
                Ok(actual)
            },
        )
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(loaded.file().len(), 256);
        assert_eq!(loaded.overlay().summary().through().number(), 2);
        let entry = loaded
            .overlay()
            .get(Key::Blob(BlobId::from_bytes([0x44; 16])))
            .unwrap()
            .unwrap();
        assert_eq!(entry.sequence.number(), 2);
        assert_eq!(entry.ordinal, 0);
        assert!(matches!(entry.mutation, Mutation::Delete(_)));
        let mut read = [0; 1];
        assert_eq!(loaded.file().read_at(256, &mut read).unwrap(), 0);
    }
    #[test]
    fn admission_and_identity_refuse_before_open() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let name = Name::account(ACCOUNT, AccountEntry::Journal(Number::new(2).unwrap())).unwrap();
        std::fs::remove_file(fixture.path.join(name.as_path().unwrap())).unwrap();
        let view = bytes.view(2, 256);
        let mut frames = [0; 160];
        let mut cells = [Cell::EMPTY; 2];
        let mut wrong = view;
        wrong.generation += 1;
        assert!(matches!(
            root.load_active_overlay(
                &Provider,
                bytes.selection(),
                wrong,
                256,
                &mut frames,
                &mut cells
            ),
            Err(OverlayInputError::Stream(_))
        ));
        assert!(
            matches!(root.load_active_overlay(&Provider,bytes.selection(),view,255,&mut frames,&mut cells),Err(OverlayInputError::Io(e)) if e.kind()==io::ErrorKind::InvalidInput)
        );
        assert!(matches!(
            root.load_active_overlay(
                &Provider,
                bytes.selection(),
                view,
                256,
                frames.get_mut(..159).unwrap(),
                &mut cells
            ),
            Err(OverlayInputError::Io(e)) if e.kind() == io::ErrorKind::InvalidInput
        ));
        assert!(
            matches!(root.load_active_overlay(&Provider,bytes.selection(),view,256,&mut frames,&mut []),
            Err(OverlayInputError::Io(e)) if e.kind()==io::ErrorKind::InvalidInput)
        );
        let mut too_many = vec![Cell::EMPTY; MAX_JOURNAL_OPERATIONS + 1];
        assert!(
            matches!(root.load_active_overlay(&Provider,bytes.selection(),view,256,&mut frames,&mut too_many),Err(OverlayInputError::Io(e)) if e.kind()==io::ErrorKind::InvalidInput)
        );
        let mut too_large = vec![0; MAX_JOURNAL_FRAME_BYTES + 1];
        assert!(
            matches!(root.load_active_overlay(&Provider,bytes.selection(),view,256,&mut too_large,&mut cells),Err(OverlayInputError::Io(e)) if e.kind()==io::ErrorKind::InvalidInput)
        );
        assert!(
            matches!(root.load_active_overlay(&Provider,bytes.selection(),view,256,&mut frames,&mut cells),Err(OverlayInputError::Io(e)) if e.kind()==io::ErrorKind::NotFound)
        );
    }
    #[test]
    fn selected_header_frames_and_final_sequence_must_match() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut frames = [0; 320];
        let mut cells = [Cell::EMPTY; 4];
        let original = journal();
        let mut wrong = original.clone();
        let mut header = JournalHeader::decode(&Provider, wrong.get(..96).unwrap()).unwrap();
        header.account = AccountId::from_bytes([9; 16]);
        header
            .encode(&Provider, wrong.get_mut(..96).unwrap())
            .unwrap();
        write(&root, &wrong);
        let mut calls = 0;
        assert!(matches!(
            load_using(
                &Provider,
                bytes.selection(),
                bytes.view(2, 256),
                input(&root, 256),
                frames.get_mut(..160).unwrap(),
                &mut cells,
                |file, output| {
                    calls += 1;
                    file.read(output)
                }
            ),
            Err(OverlayInputError::Stream(_))
        ));
        assert_eq!(calls, 1);
        write(&root, &original);
        assert!(matches!(
            root.load_active_overlay(
                &Provider,
                bytes.selection(),
                bytes.view(2, 255),
                255,
                &mut frames,
                &mut cells
            ),
            Err(OverlayInputError::Stream(StreamError::Frame(
                DecodeError::Incomplete { .. }
            )))
        ));
        let mut corrupt = original.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        write(&root, &corrupt);
        assert!(matches!(
            root.load_active_overlay(
                &Provider,
                bytes.selection(),
                bytes.view(2, 256),
                256,
                &mut frames,
                &mut cells
            ),
            Err(OverlayInputError::Stream(StreamError::Frame(
                DecodeError::Invalid(_)
            )))
        ));
        let mut data = original.clone();
        let mut second = original.get(96..).unwrap().to_vec();
        crate::format::frame::seal(&Provider, Sequence::from_u64(3), 2, &mut second).unwrap();
        data.extend_from_slice(&second);
        write(&root, &data);
        assert!(matches!(
            root.load_active_overlay(
                &Provider,
                bytes.selection(),
                bytes.view(2, 416),
                416,
                &mut frames,
                &mut cells
            ),
            Err(OverlayInputError::Stream(StreamError::Journal(_)))
        ));
        let loaded = root
            .load_active_overlay(
                &Provider,
                bytes.selection(),
                bytes.view(3, 416),
                416,
                &mut frames,
                &mut cells,
            )
            .unwrap();
        assert_eq!(loaded.overlay().operation_count(), 4);
        assert_eq!(
            loaded
                .overlay()
                .get(Key::Blob(BlobId::from_bytes([0x44; 16])))
                .unwrap()
                .unwrap()
                .sequence
                .number(),
            3
        );
    }
    #[test]
    fn shared_read_budget_and_io_failures_return_no_partial_overlay() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut frames = [0; 160];
        let mut cells = [Cell::EMPTY; 2];
        for mode in 0..6 {
            write(&root, &journal());
            let mut calls = 0;
            let result = load_using(
                &Provider,
                bytes.selection(),
                bytes.view(2, 256),
                input(&root, 256),
                &mut frames,
                &mut cells,
                |file, output| {
                    calls += 1;
                    match mode {
                        0 => file.read(output.get_mut(..1).unwrap()),
                        1 => Err(io::ErrorKind::Interrupted.into()),
                        2 => Ok(0),
                        3 => Ok(output.len() + 1),
                        4 => {
                            write(&root, &[]);
                            file.read(output)
                        }
                        _ => {
                            let n = file.read(output)?;
                            if calls == 2 {
                                write(&root, &[]);
                            }
                            Ok(n)
                        }
                    }
                },
            );
            let expected = match mode {
                0 => io::ErrorKind::WouldBlock,
                1 => io::ErrorKind::Interrupted,
                2 | 4 => io::ErrorKind::UnexpectedEof,
                _ => io::ErrorKind::InvalidData,
            };
            assert!(matches!(result,Err(OverlayInputError::Io(e)) if e.kind()==expected));
            assert_eq!(
                calls,
                match mode {
                    0 => MAX_READ_CALLS,
                    5 => 2,
                    _ => 1,
                }
            );
        }
        write(&root, &journal());
        let mut calls = 0;
        let loaded = load_using(
            &Provider,
            bytes.selection(),
            bytes.view(2, 256),
            input(&root, 256),
            &mut frames,
            &mut cells,
            |file, output| {
                calls += 1;
                let count = output.len().min(2);
                file.read(output.get_mut(..count).unwrap())
            },
        )
        .unwrap();
        assert_eq!(calls, MAX_READ_CALLS);
        assert_eq!(loaded.overlay().operation_count(), 2);
    }
}
