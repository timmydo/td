//! One validated frame appended and synced under externally held writer admission.
use super::super::{open_extent_using, CompleteFile, Extent};
use super::{repair::check_current, RecoveryInputError, ScannedJournal};
use crate::{
    format::{
        container::Current, frame::Frame, journal_stream::Error as StreamError,
        Error as FormatError, Sequence, MAX_JOURNAL_FRAME_BYTES, MAX_JOURNAL_OPERATIONS,
    },
    ports::Crypto,
};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    os::unix::fs::FileExt,
};

#[path = "append/reserved.rs"]
mod reserved;
pub use reserved::{ReconciledAppend, ReservedAppend, ReservedAppendError};

const MAX_WRITE_CALLS: usize = 64;
#[derive(Debug)]
pub enum AppendError {
    /// Constructor refusal proves this append wrote no bytes.
    Rejected(RecoveryInputError),
    /// Stop the writer and retain charges until recovery accounts for effects.
    Indeterminate(io::Error),
    /// A previous step failed; effects remain uncertain until recovery.
    Failed,
    /// Finish preceded confirmation; even a fully synced frame remains uncertain.
    Incomplete,
}
impl std::fmt::Display for AppendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(e) => write!(f, "journal append refused before writes: {e}"),
            Self::Indeterminate(e) => write!(f, "journal append effects uncertain: {e}"),
            Self::Failed => f.write_str("journal append retired with uncertain effects"),
            Self::Incomplete => f.write_str("journal append incomplete with uncertain effects"),
        }
    }
}
impl std::error::Error for AppendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rejected(e) => Some(e),
            Self::Indeterminate(e) => Some(e),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppendStep {
    Written { bytes: usize },
    Synced,
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Writing,
    Sync,
    Confirm,
    Complete,
}
/// Dropping unfinished work leaves any bytes in place; caller stops the writer for recovery.
#[must_use = "finish the append or stop the writer for recovery"]
pub struct JournalAppend<'r, 'b> {
    file: CompleteFile<'r>,
    current: Current,
    bytes: &'b [u8],
    through: Sequence,
    end: u64,
    total_operations: usize,
    operations: usize,
    written: usize,
    calls: usize,
    phase: Phase,
    failed: bool,
}
impl<'r> ScannedJournal<'r> {
    /// Caller admits full frame validation, owns its reservation, and keeps actual
    /// stopped-store exclusion through completion (no live readers or writers).
    /// The stable private namespace remains required; no final-row/blob policy is granted.
    ///
    /// ```compile_fail,E0502
    /// use td_mta::store_fs::ScannedJournal;
    /// fn overwrite(scan: ScannedJournal<'_>, mut bytes: Vec<u8>) {
    ///     let mut append = scan.append_frame(&td_crypto::Provider, &bytes).unwrap();
    ///     bytes.clear();
    ///     append.advance().unwrap();
    /// }
    /// ```
    pub fn append_frame<'b>(
        self,
        crypto: &impl Crypto,
        bytes: &'b [u8],
    ) -> Result<JournalAppend<'r, 'b>, AppendError> {
        if self.has_incomplete_tail() {
            return Err(AppendError::Rejected(
                io::Error::from(io::ErrorKind::InvalidInput).into(),
            ));
        }
        let total_bytes = self
            .summary
            .frame_bytes()
            .checked_add(bytes.len())
            .filter(|n| *n <= MAX_JOURNAL_FRAME_BYTES)
            .ok_or(AppendError::Rejected(FormatError::Limit.into()))?;
        let frame = Frame::decode(crypto, self.summary.through(), bytes)
            .map_err(|e| AppendError::Rejected(StreamError::from(e).into()))?;
        let header = frame.header();
        let total_operations = self
            .summary
            .operations()
            .checked_add(header.operations)
            .filter(|n| *n <= MAX_JOURNAL_OPERATIONS)
            .ok_or(AppendError::Rejected(FormatError::Limit.into()))?;
        let end = u64::try_from(total_bytes)
            .ok()
            .and_then(|n| n.checked_add(crate::format::JOURNAL_HEADER_BYTES as u64))
            .ok_or(AppendError::Rejected(FormatError::Overflow.into()))?;
        let owner = self.file.owner;
        check_current(owner, crypto, self.current).map_err(AppendError::Rejected)?;
        let (file, length) = open_extent_using(
            &owner.root.directory,
            &self.file.name,
            self.prefix,
            Extent::Whole,
            |path| OpenOptions::new().read(true).append(true).open(path),
        )
        .map_err(|e| AppendError::Rejected(e.into()))?;
        let original = self
            .file
            .file
            .metadata()
            .map_err(|e| AppendError::Rejected(e.into()))?;
        let opened = file
            .metadata()
            .map_err(|e| AppendError::Rejected(e.into()))?;
        if length != self.prefix
            || original.len() != self.prefix
            || opened.len() != self.prefix
            || !super::super::super::same_file(&original, &opened)
        {
            return Err(AppendError::Rejected(
                io::Error::from(io::ErrorKind::InvalidData).into(),
            ));
        }
        Ok(JournalAppend {
            file: CompleteFile {
                owner,
                file,
                name: self.file.name,
                length: self.prefix,
            },
            current: self.current,
            bytes,
            through: header.sequence,
            end,
            total_operations,
            operations: header.operations,
            written: 0,
            calls: 0,
            phase: Phase::Writing,
            failed: false,
        })
    }
}
impl<'r> JournalAppend<'r, '_> {
    pub const fn is_failed(&self) -> bool {
        self.failed
    }
    pub const fn written_bytes(&self) -> usize {
        self.written
    }
    /// At most one 64 KiB write, one sync, or final metadata/EOF checks. The caller
    /// brackets each step with its clock/work policy. Any step error retires output.
    pub fn advance(&mut self) -> Result<AppendStep, AppendError> {
        self.advance_using(&mut Real)
    }
    fn advance_using(&mut self, ops: &mut impl Operations) -> Result<AppendStep, AppendError> {
        if self.failed {
            return Err(AppendError::Failed);
        }
        if self.phase == Phase::Complete {
            return Ok(AppendStep::Complete);
        }
        self.failed = true;
        let step = self.work(ops).map_err(AppendError::Indeterminate)?;
        self.failed = false;
        Ok(step)
    }
    fn work(&mut self, ops: &mut impl Operations) -> io::Result<AppendStep> {
        match self.phase {
            Phase::Writing => {
                if self.calls >= MAX_WRITE_CALLS {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                self.calls += 1;
                let remaining = self
                    .bytes
                    .get(self.written..)
                    .ok_or(io::ErrorKind::InvalidData)?;
                let part = remaining
                    .get(
                        ..remaining
                            .len()
                            .min(super::super::super::MAX_FILE_STEP_BYTES),
                    )
                    .ok_or(io::ErrorKind::InvalidData)?;
                let count = ops.write(&mut self.file.file, part)?;
                if count == 0 {
                    return Err(io::ErrorKind::WriteZero.into());
                }
                if count > part.len() {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                self.written = self
                    .written
                    .checked_add(count)
                    .ok_or(io::ErrorKind::InvalidData)?;
                if self.written == self.bytes.len() {
                    self.phase = Phase::Sync;
                }
                Ok(AppendStep::Written { bytes: count })
            }
            Phase::Sync => {
                ops.sync(&self.file.file)?;
                self.phase = Phase::Confirm;
                Ok(AppendStep::Synced)
            }
            Phase::Confirm => {
                if ops.length(&self.file.file)? != self.end
                    || ops.read(&self.file.file, &mut [0; 1], self.end)? != 0
                {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                self.file.length = self.end;
                self.phase = Phase::Complete;
                Ok(AppendStep::Complete)
            }
            Phase::Complete => Ok(AppendStep::Complete),
        }
    }
    pub fn finish(self) -> Result<SyncedAppend<'r>, AppendError> {
        if self.failed {
            return Err(AppendError::Failed);
        }
        if self.phase != Phase::Complete {
            return Err(AppendError::Incomplete);
        }
        Ok(SyncedAppend {
            file: self.file,
            current: self.current,
            through: self.through,
            frame_bytes: self.bytes.len(),
            total_operations: self.total_operations,
            operations: self.operations,
        })
    }
}
/// Durable append evidence, not published visibility, final graph validity or acknowledgment.
pub struct SyncedAppend<'r> {
    file: CompleteFile<'r>,
    current: Current,
    through: Sequence,
    frame_bytes: usize,
    total_operations: usize,
    operations: usize,
}
impl SyncedAppend<'_> {
    pub const fn current(&self) -> Current {
        self.current
    }
    pub const fn through(&self) -> Sequence {
        self.through
    }
    pub fn end(&self) -> u64 {
        self.file.len()
    }
    pub const fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }
    pub const fn operations(&self) -> usize {
        self.operations
    }
    pub const fn total_operations(&self) -> usize {
        self.total_operations
    }
}
trait Operations {
    fn write(&mut self, file: &mut File, bytes: &[u8]) -> io::Result<usize>;
    fn sync(&mut self, file: &File) -> io::Result<()>;
    fn length(&mut self, file: &File) -> io::Result<u64>;
    fn read(&mut self, file: &File, bytes: &mut [u8], offset: u64) -> io::Result<usize>;
}
struct Real;
impl Operations for Real {
    fn write(&mut self, file: &mut File, bytes: &[u8]) -> io::Result<usize> {
        file.write(bytes)
    }
    fn sync(&mut self, file: &File) -> io::Result<()> {
        file.sync_all()
    }
    fn length(&mut self, file: &File) -> io::Result<u64> {
        Ok(file.metadata()?.len())
    }
    fn read(&mut self, file: &File, bytes: &mut [u8], offset: u64) -> io::Result<usize> {
        file.read_at(bytes, offset)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::super::super::super::{
        active::{fixture, ProbeBytes},
        selection,
        tests::Fixture,
        LockedRoot,
    };
    use super::*;
    use crate::{
        format::{self, CURRENT_BYTES, MAX_FRAME_BYTES},
        ids::AccountId,
        store_paths::{AccountEntry, Name, Number},
    };
    use td_crypto::Provider;
    pub(super) fn setup() -> (Fixture, LockedRoot, ProbeBytes) {
        let dir = Fixture::new();
        let root = dir.locked();
        root.create_accounts_directory().unwrap();
        selection::prepare_probe(&root);
        let account = AccountId::from_bytes([0x33; 16]);
        root.create_account_directory(account, AccountEntry::Journals)
            .unwrap();
        let bytes = fixture::prepare(&root);
        let mut current = [0; CURRENT_BYTES];
        bytes
            .selection()
            .current()
            .encode(&Provider, &mut current)
            .unwrap();
        let path = Name::account(account, AccountEntry::Current).unwrap();
        std::fs::write(dir.path.join(path.as_path().unwrap()), current).unwrap();
        fixture::write(&root, &fixture::journal());
        (dir, root, bytes)
    }
    pub(super) fn scan<'r>(root: &'r LockedRoot, bytes: &ProbeBytes) -> ScannedJournal<'r> {
        let mut scratch: Box<[u8; MAX_FRAME_BYTES]> = vec![0; MAX_FRAME_BYTES]
            .into_boxed_slice()
            .try_into()
            .unwrap();
        let mut input = root
            .scan_active_journal(
                &Provider,
                bytes.selection(),
                (format::JOURNAL_HEADER_BYTES + MAX_JOURNAL_FRAME_BYTES) as u64,
                &mut scratch,
            )
            .unwrap();
        while input.next_frame().unwrap().is_some() {}
        input.finish().unwrap()
    }
    pub(super) fn next(sequence: u64) -> Vec<u8> {
        let mut frame = fixture::journal()[96..].to_vec();
        format::frame::seal(&Provider, Sequence::from_u64(sequence), 2, &mut frame).unwrap();
        frame
    }
    #[test]
    fn writes_then_syncs_then_confirms_and_only_finish_returns_evidence() {
        let (_dir, root, bytes) = setup();
        let frame = next(3);
        let mut append = scan(&root, &bytes).append_frame(&Provider, &frame).unwrap();
        assert!(std::mem::size_of_val(&append) <= 1024);
        assert_eq!(append.written_bytes(), 0);
        assert_eq!(
            append.advance().unwrap(),
            AppendStep::Written { bytes: 160 }
        );
        assert_eq!(append.written_bytes(), 160);
        assert_eq!(append.advance().unwrap(), AppendStep::Synced);
        assert_eq!(append.advance().unwrap(), AppendStep::Complete);
        assert_eq!(append.advance().unwrap(), AppendStep::Complete);
        let complete = append.finish().unwrap();
        assert!(std::mem::size_of_val(&complete) <= 1024);
        assert_eq!(complete.current(), bytes.selection().current());
        assert_eq!(complete.through(), Sequence::from_u64(3));
        assert_eq!(complete.end(), 416);
        assert_eq!(complete.frame_bytes(), 160);
        assert_eq!(complete.total_operations(), 4);
        assert_eq!(complete.operations(), 2);
        let mut all = [0; 416];
        assert_eq!(complete.file.read_at(0, &mut all).unwrap(), 416);
        assert_eq!(&all[..256], fixture::journal());
        assert_eq!(&all[256..], &frame);
        let recovered = scan(&root, &bytes);
        assert_eq!(recovered.summary().through(), complete.through());
        assert!(!recovered.has_incomplete_tail());
    }
    pub(super) struct Faults {
        mode: u8,
        writes: usize,
        syncs: usize,
        lengths: usize,
        reads: usize,
    }
    impl Faults {
        pub(super) fn new(mode: u8) -> Self {
            Self {
                mode,
                writes: 0,
                syncs: 0,
                lengths: 0,
                reads: 0,
            }
        }
        fn calls(&self) -> (usize, usize, usize, usize) {
            (self.writes, self.syncs, self.lengths, self.reads)
        }
    }
    impl Operations for Faults {
        fn write(&mut self, file: &mut File, bytes: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            assert!(bytes.len() <= super::super::super::super::MAX_FILE_STEP_BYTES);
            match self.mode {
                1 if self.writes > 1 => Err(io::ErrorKind::StorageFull.into()),
                2 => Ok(0),
                3 => Ok(bytes.len() + 1),
                9 => file.write(&bytes[..1]),
                0 | 1 => file.write(&bytes[..bytes.len().min(7)]),
                _ => file.write(bytes),
            }
        }
        fn sync(&mut self, file: &File) -> io::Result<()> {
            self.syncs += 1;
            if self.mode == 4 {
                Err(io::ErrorKind::Other.into())
            } else {
                file.sync_all()
            }
        }
        fn length(&mut self, file: &File) -> io::Result<u64> {
            self.lengths += 1;
            if self.mode == 5 {
                return Err(io::ErrorKind::Other.into());
            }
            Ok(file.metadata()?.len() + u64::from(self.mode == 6))
        }
        fn read(&mut self, file: &File, bytes: &mut [u8], offset: u64) -> io::Result<usize> {
            self.reads += 1;
            match self.mode {
                7 => Err(io::ErrorKind::Other.into()),
                8 => Ok(1),
                _ => file.read_at(bytes, offset),
            }
        }
    }
    #[test]
    fn short_writes_and_every_write_sync_confirmation_failure_keep_the_old_prefix() {
        for mode in 0..10 {
            let (_dir, root, bytes) = setup();
            let frame = next(3);
            let mut append = scan(&root, &bytes).append_frame(&Provider, &frame).unwrap();
            let mut ops = Faults::new(mode);
            let mut outcome = None;
            for _ in 0..70 {
                match append.advance_using(&mut ops) {
                    Ok(AppendStep::Complete) => {
                        outcome = Some(Ok(()));
                        break;
                    }
                    Ok(_) => (),
                    Err(error) => {
                        outcome = Some(Err(error));
                        break;
                    }
                }
            }
            let outcome = outcome.unwrap();
            if mode == 0 {
                outcome.unwrap();
                assert_eq!(ops.writes, 23);
                let calls = ops.calls();
                assert_eq!(
                    append.advance_using(&mut ops).unwrap(),
                    AppendStep::Complete
                );
                assert_eq!(ops.calls(), calls);
                assert_eq!(append.finish().unwrap().end(), 416);
            } else {
                let expected = match mode {
                    1 => io::ErrorKind::StorageFull,
                    2 => io::ErrorKind::WriteZero,
                    3 | 6 | 8 => io::ErrorKind::InvalidData,
                    9 => io::ErrorKind::WouldBlock,
                    _ => io::ErrorKind::Other,
                };
                assert!(
                    matches!(outcome, Err(AppendError::Indeterminate(ref e)) if e.kind() == expected)
                );
                assert!(append.is_failed());
                let calls = ops.calls();
                assert!(matches!(
                    append.advance_using(&mut ops),
                    Err(AppendError::Failed)
                ));
                assert_eq!(ops.calls(), calls);
                assert!(matches!(append.finish(), Err(AppendError::Failed)));
                if mode == 9 {
                    assert_eq!(ops.writes, MAX_WRITE_CALLS);
                }
            }
            let recovered = scan(&root, &bytes);
            let mut prefix = [0; 256];
            recovered.file.read_at(0, &mut prefix).unwrap();
            assert_eq!(prefix.as_slice(), fixture::journal());
            assert_eq!(
                recovered.summary().through().number(),
                if mode == 0 || (4..=8).contains(&mode) {
                    3
                } else {
                    2
                }
            );
            assert_eq!(recovered.has_incomplete_tail(), mode == 1 || mode == 9);
        }
    }
    #[test]
    fn premature_finish_and_drop_never_sync_or_claim_success() {
        for steps in 0..3 {
            let (_dir, root, bytes) = setup();
            let frame = next(3);
            let mut append = scan(&root, &bytes).append_frame(&Provider, &frame).unwrap();
            let mut ops = Faults::new(10);
            for _ in 0..steps {
                append.advance_using(&mut ops).unwrap();
            }
            assert!(matches!(append.finish(), Err(AppendError::Incomplete)));
            assert_eq!(ops.syncs, usize::from(steps == 2));
            assert_eq!(ops.lengths, 0);
            let recovered = scan(&root, &bytes);
            assert_eq!(
                recovered.summary().through().number(),
                if steps == 0 { 2 } else { 3 }
            );
        }
    }
    #[test]
    fn incomplete_tail_bad_frame_wrong_sequence_and_changed_selection_or_file_refuse_before_write()
    {
        for mode in 0..6 {
            let (dir, root, bytes) = setup();
            let mut frame = next(if mode == 2 { 4 } else { 3 });
            if mode == 0 {
                fixture::append(&root, b"tail");
            }
            let scanned = scan(&root, &bytes);
            let path = dir.path.join(
                Name::account(
                    bytes.selection().current().account,
                    AccountEntry::Journal(Number::new(2).unwrap()),
                )
                .unwrap()
                .as_path()
                .unwrap(),
            );
            if mode == 1 {
                *frame.last_mut().unwrap() ^= 1;
            }
            if mode == 3 {
                let current =
                    Name::account(bytes.selection().current().account, AccountEntry::Current)
                        .unwrap();
                std::fs::write(
                    dir.path.join(current.as_path().unwrap()),
                    [0; CURRENT_BYTES],
                )
                .unwrap();
            }
            if mode == 4 {
                fixture::append(&root, b"late");
            }
            if mode == 5 {
                std::fs::remove_file(&path).unwrap();
                fixture::write(&root, &fixture::journal());
            }
            let original = std::fs::read(&path).unwrap();
            let refused = scanned.append_frame(&Provider, &frame);
            if mode == 0 {
                assert!(
                    matches!(refused, Err(AppendError::Rejected(RecoveryInputError::Io(ref e))) if e.kind() == io::ErrorKind::InvalidInput)
                );
            } else {
                assert!(matches!(refused, Err(AppendError::Rejected(_))));
            }
            assert_eq!(std::fs::read(path).unwrap(), original);
        }
    }
    pub(super) fn many(sequence: u64, count: usize, large: bool) -> Vec<u8> {
        use crate::format::{
            operation::Operation,
            row::{MailboxRow, Row, MAX_MAILBOX_NAME},
            Table,
        };
        let key = [0x77; 16];
        let name = "n".repeat(MAX_MAILBOX_NAME);
        let mut value = [0; 2048];
        let length = Row::Mailbox(MailboxRow {
            name: &name,
            parent: None,
            role: None,
            sort_order: 0,
            subscribed: false,
        })
        .encode(&mut value)
        .unwrap();
        let operation = if large {
            Operation::put(Table::Mailboxes, &key, &value[..length]).unwrap()
        } else {
            Operation::delete(Table::Mailboxes, &key).unwrap()
        };
        let size = operation.encoded_len().unwrap();
        let end = format::FRAME_HEADER_BYTES + size * count;
        let mut frame = vec![0; end + format::FRAME_FOOTER_BYTES];
        for bytes in frame[format::FRAME_HEADER_BYTES..end].chunks_exact_mut(size) {
            operation.encode(bytes).unwrap();
        }
        format::frame::seal(&Provider, Sequence::from_u64(sequence), count, &mut frame).unwrap();
        frame
    }
    #[test]
    fn large_frames_use_bounded_writes_and_cumulative_byte_operation_caps_precede_append() {
        let (_dir, root, bytes) = setup();
        let frame = many(3, 900, true);
        assert!(frame.len() > 900_000);
        let mut append = scan(&root, &bytes).append_frame(&Provider, &frame).unwrap();
        let mut written = 0;
        let mut calls = 0;
        while let AppendStep::Written { bytes } = append.advance().unwrap() {
            assert!(bytes <= super::super::super::super::MAX_FILE_STEP_BYTES);
            written += bytes;
            calls += 1;
        }
        assert_eq!(written, frame.len());
        assert_eq!(
            calls,
            frame
                .len()
                .div_ceil(super::super::super::super::MAX_FILE_STEP_BYTES)
        );
        assert_eq!(append.advance().unwrap(), AppendStep::Complete);
        assert_eq!(append.finish().unwrap().frame_bytes(), frame.len());
        for large in [false, true] {
            let mut journal = fixture::journal()[..96].to_vec();
            let (frames, operations) = if large { (4, 900) } else { (2, 4096) };
            for sequence in 2..2 + frames {
                journal.extend_from_slice(&many(sequence, operations, large));
            }
            assert!(journal.len() - 96 <= MAX_JOURNAL_FRAME_BYTES);
            fixture::write(&root, &journal);
            let scanned = scan(&root, &bytes);
            let candidate = many(2 + frames, operations, large);
            assert!(matches!(
                scanned.append_frame(&Provider, &candidate),
                Err(AppendError::Rejected(RecoveryInputError::Stream(
                    StreamError::Journal(crate::format::container::Error::Format(
                        FormatError::Limit
                    ))
                )))
            ));
            let recovered = scan(&root, &bytes);
            assert_eq!(recovered.file.len(), journal.len() as u64);
        }
    }
}
