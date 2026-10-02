//! Pin-bound sequential integrity checking followed by bounded immutable reads.
use super::super::super::super::super::{BlobInput, BlobInputError, CompleteBlob};
use super::super::VerifyClock;
use super::{PinnedReadError, PinnedReadRequest, PooledRead};
use crate::{
    format::{
        key::Key,
        row::{BlobKind, Row},
    },
    ids::BlobId,
    ports::{BlobReader, Clock, Crypto, Error as PolicyError},
};
use std::sync::atomic::AtomicU64;

fn blob_io_error(error: std::io::Error) -> PolicyError {
    match error.kind() {
        std::io::ErrorKind::NotFound
        | std::io::ErrorKind::UnexpectedEof
        | std::io::ErrorKind::InvalidData => PolicyError::Corrupt,
        _ => error.into(),
    }
}
fn blob_error(error: BlobInputError) -> PolicyError {
    match error {
        BlobInputError::Io(error) => blob_io_error(error),
        BlobInputError::Crypto(error) => error.into(),
        BlobInputError::Checksum => PolicyError::Corrupt,
    }
}
fn checked<T>(
    clock: &VerifyClock<'_>,
    run: impl FnOnce() -> Result<T, PolicyError>,
) -> Result<T, PolicyError> {
    clock.sample()?;
    let result = run();
    clock.sample()?;
    result
}
impl PooledRead<'_, '_, '_, '_, '_> {
    /// Caller authorizes this root blob before entry. Lookup uses this pin's view;
    /// bytes stay provisional until finish. The input exclusively borrows the pin.
    /// Each read processes at most 64 KiB; the query deadline covers all phases.
    ///
    /// ```compile_fail
    /// use td_mta::{ids::BlobId, ports::{Clock, BlobReader},
    ///     store_fs::{PooledRead, PinnedReadRequest}};
    /// fn release(mut view: PooledRead<'_, '_, '_, '_, '_>, clock: &dyn Clock,
    ///     request: PinnedReadRequest, id: BlobId) {
    ///     let input = view.open_blob_input(&td_crypto::Provider, clock, request, id, 3).unwrap();
    ///     let mut body = input.finish().unwrap();
    ///     drop(view);
    ///     body.read_at(0, &mut [0; 3]).unwrap();
    /// }
    /// ```
    pub fn open_blob_input<'a, 'c, C: Crypto>(
        &'a mut self,
        crypto: &'c C,
        clock: &'c dyn Clock,
        request: PinnedReadRequest,
        id: BlobId,
        max_bytes: u64,
    ) -> Result<PinnedBlobInput<'a, 'c, C>, PinnedReadError>
    where
        C::Sha256: Sync,
    {
        if max_bytes > i64::MAX as u64
            || std::mem::size_of::<PinnedBlobInput<'_, '_, C>>()
                .checked_add(std::mem::size_of::<PinnedBlob<'_, '_>>())
                .is_none_or(|bytes| bytes > 4 * 1024)
        {
            return Err(PinnedReadError::Policy(PolicyError::Invalid));
        }
        let clock = VerifyClock {
            source: clock,
            deadline: request.query.deadline,
            last: AtomicU64::new(0),
        };
        let mut value = [0; 64];
        let row = self.with_read_view(crypto, &clock, request, |view| {
            match view.get(Key::Blob(id), &mut value)? {
                Some((Row::Blob(row), _)) => Ok(row),
                Some(_) => Err(PolicyError::Corrupt),
                None => Err(PolicyError::NotFound),
            }
        })?;
        let input = checked(&clock, || {
            if row.length > max_bytes {
                return Err(PolicyError::Capacity);
            }
            self.pin
                .session
                .store
                .store
                .root
                .open_blob_input(crypto, self.identity().account, id, row, max_bytes)
                .map_err(blob_error)
        })
        .map_err(PinnedReadError::Policy)?;
        Ok(PinnedBlobInput {
            input,
            clock,
            failed: None,
        })
    }
}
/// Retains the pooled view borrow; an error retires this input without more I/O.
pub struct PinnedBlobInput<'a, 'c, C: Crypto> {
    input: BlobInput<'a, 'c, C>,
    clock: VerifyClock<'c>,
    failed: Option<PolicyError>,
}
impl<'a, 'c, C: Crypto> PinnedBlobInput<'a, 'c, C> {
    pub fn len(&self) -> u64 {
        self.input.len()
    }
    pub fn is_empty(&self) -> bool {
        self.input.is_empty()
    }
    pub fn position(&self) -> u64 {
        self.input.position()
    }
    pub fn is_failed(&self) -> bool {
        self.failed.is_some()
    }
    /// Errors may overwrite output; no returned byte is verified until finish.
    pub fn read(&mut self, output: &mut [u8]) -> Result<usize, PolicyError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = checked(&self.clock, || self.input.read(output).map_err(blob_error));
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    /// Observe exact EOF and digest before lending random reads of the same file.
    pub fn finish(self) -> Result<PinnedBlob<'a, 'c>, PolicyError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let complete = checked(&self.clock, || self.input.finish().map_err(blob_error))?;
        Ok(PinnedBlob {
            complete,
            clock: self.clock,
            failed: None,
        })
    }
}
/// One checked immutable descriptor. Its lifetime retains the pooled view borrow.
pub struct PinnedBlob<'a, 'c> {
    complete: CompleteBlob<'a>,
    clock: VerifyClock<'c>,
    failed: Option<PolicyError>,
}
impl PinnedBlob<'_, '_> {
    pub fn id(&self) -> BlobId {
        self.complete.id()
    }
    pub fn kind(&self) -> BlobKind {
        self.complete.kind()
    }
    pub fn digest(&self) -> &[u8; 32] {
        self.complete.digest()
    }
    pub fn is_failed(&self) -> bool {
        self.failed.is_some()
    }
}
impl BlobReader for PinnedBlob<'_, '_> {
    fn len(&self) -> u64 {
        self.complete.file().len()
    }
    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, PolicyError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = checked(&self.clock, || {
            self.complete
                .file()
                .read_at(offset, output)
                .map_err(blob_io_error)
        });
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
}

#[cfg(test)]
pub use tests::probe as probe_pinned_blobs;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::super::super::super::super::super::tests::Fixture;
    use super::super::super::super::tests::owned_blob_fixture_with;
    use super::super::super::{
        tests::{
            budget, deadline, frame, read_request, start, with_ledger_usage, CommitFixture,
            TestClock,
        },
        JournalSession,
    };
    use super::super::{tests::Backing, ReadScratchPool, ReadScratchSlot};
    use super::*;
    use crate::{
        limits::Limits,
        ports::{Tick, Time},
        store_paths::{AccountEntry, Name},
    };
    use std::sync::atomic::Ordering;
    use td_crypto::Provider;
    const ID: BlobId = BlobId::from_bytes([0x44; 16]);
    fn with_blob(
        bytes: &[u8],
        kind: BlobKind,
        run: impl FnOnce(&Fixture, &ReadScratchPool<'_, '_>, &JournalSession<'_, '_>),
    ) {
        with_blob_at(Fixture::new(), bytes, kind, run);
    }
    fn with_blob_at(
        dir: Fixture,
        bytes: &[u8],
        kind: BlobKind,
        run: impl FnOnce(&Fixture, &ReadScratchPool<'_, '_>, &JournalSession<'_, '_>),
    ) {
        let (dir, verified, mut startup) = owned_blob_fixture_with(dir, bytes, kind);
        let summary = verified.journal();
        let plan = Limits::default().plan().unwrap();
        let mut a = Backing::new(&plan);
        let mut b = Backing::new(&plan);
        let mut slots = [
            ReadScratchSlot::new(&plan, a.borrow()).unwrap(),
            ReadScratchSlot::new(&plan, b.borrow()).unwrap(),
        ];
        let pool = ReadScratchPool::new(&plan, &mut slots).unwrap();
        with_ledger_usage(
            summary.frame_bytes() as u64,
            summary.operations() as u64,
            |ledger| {
                verified
                    .with_journal(
                        &Provider,
                        &TestClock::new(u64::MAX),
                        ledger,
                        start(),
                        startup.journal(),
                        |session, _| run(&dir, &pool, session),
                    )
                    .unwrap();
            },
        );
    }
    fn path(dir: &Fixture, session: &JournalSession<'_, '_>, kind: BlobKind) -> std::path::PathBuf {
        dir.path.join(
            Name::account(
                session.store.identity().account,
                AccountEntry::Blob(kind, ID),
            )
            .unwrap()
            .as_path()
            .unwrap(),
        )
    }
    #[derive(Clone, Copy)]
    enum ProbeCase {
        Read,
        Empty,
        Append,
        NotFound,
        Capacity,
        Checksum,
        Truncate,
        InputDeadline,
        FinishDeadline,
        ReadDeadline,
    }
    /// Fixture creation, full backing, verification and session startup stay cold.
    pub fn probe(mut snapshot: impl FnMut()) {
        use std::os::unix::fs::FileExt;
        for maximum in [false, true] {
            for case in [
                ProbeCase::Read,
                ProbeCase::Empty,
                ProbeCase::Append,
                ProbeCase::NotFound,
                ProbeCase::Capacity,
                ProbeCase::Checksum,
                ProbeCase::Truncate,
                ProbeCase::InputDeadline,
                ProbeCase::FinishDeadline,
                ProbeCase::ReadDeadline,
            ] {
                let fixture = if maximum {
                    Fixture::maximum_root()
                } else {
                    Fixture::new()
                };
                let bytes: &[u8] = if matches!(case, ProbeCase::Empty) {
                    b""
                } else {
                    b"abc"
                };
                let kind = if matches!(case, ProbeCase::Empty) {
                    BlobKind::Upload
                } else {
                    BlobKind::Message
                };
                with_blob_at(fixture, bytes, kind, |dir, pool, session| {
                    let clock = FaultClock::new(0);
                    let good = TestClock::new(u64::MAX);
                    let next_frame = frame(4);
                    let file = std::fs::OpenOptions::new()
                        .write(true)
                        .open(path(dir, session, kind))
                        .unwrap();
                    if matches!(case, ProbeCase::Checksum) {
                        assert_eq!(file.write_at(b"abd", 0).unwrap(), 3);
                    }
                    snapshot();
                    let mut old = pool.capture(session).unwrap();
                    let id = if matches!(case, ProbeCase::NotFound) {
                        BlobId::from_bytes([0x45; 16])
                    } else {
                        ID
                    };
                    let max = if matches!(case, ProbeCase::Capacity) {
                        2
                    } else {
                        bytes.len() as u64
                    };
                    let opened = old.open_blob_input(&Provider, &clock, read_request(), id, max);
                    match case {
                        ProbeCase::NotFound => assert!(matches!(
                            opened,
                            Err(PinnedReadError::Policy(PolicyError::NotFound))
                        )),
                        ProbeCase::Capacity => assert!(matches!(
                            opened,
                            Err(PinnedReadError::Policy(PolicyError::Capacity))
                        )),
                        _ => {
                            let mut input = opened.unwrap();
                            if matches!(case, ProbeCase::Append) {
                                session
                                    .commit(&Provider, &good, deadline(), &next_frame, budget())
                                    .unwrap();
                            }
                            if matches!(case, ProbeCase::Truncate) {
                                file.set_len(1).unwrap();
                            }
                            if matches!(case, ProbeCase::InputDeadline) {
                                clock.arm(1);
                            }
                            let mut output = [0; 3];
                            let result = input.read(&mut output);
                            match case {
                                ProbeCase::InputDeadline => {
                                    assert_eq!(result, Err(PolicyError::Deadline));
                                    assert_eq!(input.read(&mut output), Err(PolicyError::Deadline));
                                    assert!(matches!(input.finish(), Err(PolicyError::Deadline)));
                                }
                                ProbeCase::Truncate => {
                                    assert_eq!(result.unwrap(), 1);
                                    let error = input.read(&mut output).unwrap_err();
                                    assert_eq!(error, PolicyError::Corrupt);
                                    assert_eq!(input.read(&mut output), Err(error));
                                    assert!(matches!(input.finish(), Err(e) if e == error));
                                }
                                _ => {
                                    assert_eq!(result.unwrap(), bytes.len());
                                    if matches!(case, ProbeCase::FinishDeadline) {
                                        clock.arm(1);
                                    }
                                    let finished = input.finish();
                                    match case {
                                        ProbeCase::Checksum => {
                                            assert!(matches!(finished, Err(PolicyError::Corrupt)))
                                        }
                                        ProbeCase::FinishDeadline => {
                                            assert!(matches!(finished, Err(PolicyError::Deadline)))
                                        }
                                        _ => {
                                            let mut body = finished.unwrap();
                                            assert_eq!(body.id(), ID);
                                            assert_eq!(body.len(), bytes.len() as u64);
                                            assert_eq!(body.kind(), kind);
                                            if matches!(case, ProbeCase::ReadDeadline) {
                                                clock.arm(1);
                                            }
                                            let result = body.read_at(0, &mut output);
                                            if matches!(case, ProbeCase::ReadDeadline) {
                                                assert_eq!(result, Err(PolicyError::Deadline));
                                                assert_eq!(
                                                    body.read_at(0, &mut output),
                                                    Err(PolicyError::Deadline)
                                                );
                                            } else {
                                                assert_eq!(result.unwrap(), bytes.len());
                                                assert_eq!(
                                                    output.get(..bytes.len()).unwrap(),
                                                    bytes
                                                );
                                            }
                                            drop(body);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    let present = |view: &mut dyn crate::ports::ReadView| {
                        let mut value = [0; 64];
                        Ok(view.get(Key::Blob(ID), &mut value)?.is_some())
                    };
                    assert!(old
                        .with_read_view(&Provider, &good, read_request(), present)
                        .unwrap());
                    let mut new = pool.capture(session).unwrap();
                    assert_eq!(
                        new.with_read_view(&Provider, &good, read_request(), present)
                            .unwrap(),
                        !matches!(case, ProbeCase::Append)
                    );
                    drop(new);
                    drop(old);
                    snapshot();
                });
            }
        }
    }
    #[test]
    fn transfer_input_borrows_verified_body_with_bounded_extent() {
        with_blob(
            b"prefixT!Q==Zsuffix",
            BlobKind::Message,
            |_dir, pool, session| {
                let clock = TestClock::new(u64::MAX);
                let mut view = pool.capture(session).unwrap();
                let mut input = view
                    .open_blob_input(&Provider, &clock, read_request(), ID, 18)
                    .unwrap();
                assert_eq!(input.read(&mut [0; 18]).unwrap(), 18);
                let mut body = input.finish().unwrap();
                let mut backing = [0; 2];
                let mut reader = crate::mime_input::Reader::new(
                    &mut body,
                    6,
                    6,
                    crate::wire::TransferEncoding::Base64,
                    &mut backing,
                )
                .unwrap();
                let mut meter = crate::admission::work::Meter::new(
                    deadline(),
                    crate::admission::work::Charge {
                        io_bytes: 100,
                        output_bytes: 100,
                        ..crate::admission::work::Charge::default()
                    },
                );
                let mut written = 0;
                let mut complete = false;
                for _ in 0..20 {
                    let mut output = [0; 1];
                    let step = reader.poll(&clock, &mut meter, &mut output).unwrap();
                    if step.written != 0 {
                        assert_eq!(output, *b"M");
                        written += step.written;
                    }
                    if step.status == crate::mime_base64::Status::Complete {
                        complete = true;
                        break;
                    }
                }
                assert!(complete);
                assert_eq!(written, 1);
                assert_eq!(reader.position(), 1);
                assert!(reader.is_encoding_problem());
                assert_eq!(body.read_at(0, &mut [0; 6]).unwrap(), 6);
            },
        );
    }
    #[test]
    fn body_keeps_old_identity_across_delete_and_releases_pool_on_drop() {
        with_blob(b"abc", BlobKind::Message, |_dir, pool, session| {
            let clock = TestClock::new(u64::MAX);
            let mut view = pool.capture(session).unwrap();
            let mut input = view
                .open_blob_input(&Provider, &clock, read_request(), ID, 3)
                .unwrap();
            assert_eq!(input.len(), 3);
            assert!(!input.is_empty());
            assert_eq!(input.read(&mut []).unwrap(), 0);
            session
                .commit_fixture(&Provider, &clock, deadline(), &frame(4), budget())
                .unwrap();
            let mut new = pool.capture(session).unwrap();
            assert!(matches!(
                new.open_blob_input(&Provider, &clock, read_request(), ID, 3),
                Err(PinnedReadError::Policy(PolicyError::NotFound))
            ));
            let mut bytes = [0; 2];
            assert_eq!(input.read(&mut bytes).unwrap(), 2);
            assert_eq!(&bytes, b"ab");
            assert_eq!(input.read(&mut bytes).unwrap(), 1);
            assert_eq!(bytes[0], b'c');
            assert_eq!(input.position(), 3);
            let mut body = input.finish().unwrap();
            assert_eq!(body.id(), ID);
            assert_eq!(body.kind(), BlobKind::Message);
            assert_eq!(body.len(), 3);
            assert_eq!(body.read_at(1, &mut bytes).unwrap(), 2);
            assert_eq!(&bytes, b"bc");
            assert_eq!(body.read_at(3, &mut bytes).unwrap(), 0);
            assert!(!body.is_failed());
            drop(body);
            assert_eq!(view.identity().committed_sequence.number(), 3);
            drop(view);
            drop(new);
            assert!(pool.capture(session).is_ok());
        });
    }
    #[test]
    fn empty_upload_and_large_message_use_bounded_steps() {
        with_blob(b"", BlobKind::Upload, |_dir, pool, session| {
            let clock = TestClock::new(u64::MAX);
            let mut view = pool.capture(session).unwrap();
            let input = view
                .open_blob_input(&Provider, &clock, read_request(), ID, 0)
                .unwrap();
            assert!(input.is_empty());
            let mut body = input.finish().unwrap();
            assert!(body.is_empty());
            assert_eq!(body.kind(), BlobKind::Upload);
            assert_eq!(body.read_at(0, &mut [0; 1]).unwrap(), 0);
        });
        let bytes = vec![0x5a; 65537];
        with_blob(&bytes, BlobKind::Message, |_dir, pool, session| {
            let clock = TestClock::new(u64::MAX);
            let mut view = pool.capture(session).unwrap();
            let mut input = view
                .open_blob_input(&Provider, &clock, read_request(), ID, bytes.len() as u64)
                .unwrap();
            let mut out = vec![0; bytes.len()];
            assert_eq!(input.read(&mut out).unwrap(), 65536);
            assert_eq!(out.last(), Some(&0));
            assert_eq!(input.read(&mut out).unwrap(), 1);
            let mut body = input.finish().unwrap();
            out.fill(0);
            assert_eq!(body.read_at(0, &mut out).unwrap(), 65536);
            assert_eq!(out.last(), Some(&0));
        });
    }
    #[test]
    fn constructor_integrity_and_short_file_refusals_release_the_borrow() {
        with_blob(b"abc", BlobKind::Message, |dir, pool, session| {
            let clock = TestClock::new(u64::MAX);
            let mut view = pool.capture(session).unwrap();
            for (id, max, expected) in [
                (ID, 2, PolicyError::Capacity),
                (ID, u64::MAX, PolicyError::Invalid),
                (BlobId::from_bytes([0x45; 16]), 3, PolicyError::NotFound),
            ] {
                assert!(
                    matches!(view.open_blob_input(&Provider, &clock, read_request(), id, max), Err(PinnedReadError::Policy(error)) if error == expected)
                );
            }
            let input = view
                .open_blob_input(&Provider, &clock, read_request(), ID, 3)
                .unwrap();
            assert!(matches!(
                input.finish(),
                Err(PolicyError::Io {
                    kind: std::io::ErrorKind::InvalidInput,
                    ..
                })
            ));
            let file = path(dir, session, BlobKind::Message);
            std::fs::write(&file, b"abd").unwrap();
            let mut input = view
                .open_blob_input(&Provider, &clock, read_request(), ID, 3)
                .unwrap();
            assert_eq!(input.read(&mut [0; 3]).unwrap(), 3);
            assert!(matches!(input.finish(), Err(PolicyError::Corrupt)));
            std::fs::write(&file, b"abc").unwrap();
            let mut input = view
                .open_blob_input(&Provider, &clock, read_request(), ID, 3)
                .unwrap();
            std::fs::write(&file, b"a").unwrap();
            assert_eq!(input.read(&mut [0; 3]).unwrap(), 1);
            let error = input.read(&mut [0; 3]).unwrap_err();
            assert_eq!(error, PolicyError::Corrupt);
            assert!(input.is_failed());
            assert_eq!(input.read(&mut [0; 3]), Err(error));
            assert!(matches!(input.finish(), Err(e) if e == error));
            std::fs::write(&file, b"abc").unwrap();
            assert!(view
                .open_blob_input(&Provider, &clock, read_request(), ID, 3)
                .is_ok());
        });
    }
    #[test]
    fn missing_changed_extent_and_later_range_damage_are_corruption() {
        with_blob(b"abc", BlobKind::Message, |dir, pool, session| {
            let clock = TestClock::new(u64::MAX);
            let mut view = pool.capture(session).unwrap();
            let file = path(dir, session, BlobKind::Message);
            let saved = dir.path.join("saved-body");
            std::fs::rename(&file, &saved).unwrap();
            assert!(matches!(
                view.open_blob_input(&Provider, &clock, read_request(), ID, 3),
                Err(PinnedReadError::Policy(PolicyError::Corrupt))
            ));
            std::fs::rename(&saved, &file).unwrap();
            for bytes in [b"ab".as_slice(), b"abcd"] {
                std::fs::write(&file, bytes).unwrap();
                assert!(matches!(
                    view.open_blob_input(&Provider, &clock, read_request(), ID, 3),
                    Err(PinnedReadError::Policy(PolicyError::Corrupt))
                ));
            }
            std::fs::write(&file, b"abc").unwrap();
            let mut input = view
                .open_blob_input(&Provider, &clock, read_request(), ID, 3)
                .unwrap();
            input.read(&mut [0; 3]).unwrap();
            std::fs::write(&file, b"abcd").unwrap();
            assert!(matches!(input.finish(), Err(PolicyError::Corrupt)));
            for late in [false, true] {
                std::fs::write(&file, b"abc").unwrap();
                let clock = FaultClock::new(0);
                let mut input = view
                    .open_blob_input(&Provider, &clock, read_request(), ID, 3)
                    .unwrap();
                input.read(&mut [0; 3]).unwrap();
                let mut body = input.finish().unwrap();
                std::fs::write(&file, b"a").unwrap();
                if late {
                    clock.arm(1);
                }
                let expected = if late {
                    PolicyError::Deadline
                } else {
                    PolicyError::Corrupt
                };
                assert_eq!(body.read_at(2, &mut [0; 1]), Err(expected));
                let calls = clock.calls.load(Ordering::Relaxed);
                assert_eq!(body.read_at(2, &mut [0; 1]), Err(expected));
                assert_eq!(clock.calls.load(Ordering::Relaxed), calls);
            }
        });
    }
    struct FaultClock {
        calls: AtomicU64,
        fault: AtomicU64,
        mode: u8,
    }
    impl FaultClock {
        fn new(mode: u8) -> Self {
            Self {
                calls: AtomicU64::new(0),
                fault: AtomicU64::new(u64::MAX),
                mode,
            }
        }
        fn arm(&self, delta: u64) {
            self.fault.store(
                self.calls.load(Ordering::Relaxed) + delta,
                Ordering::Relaxed,
            );
        }
    }
    impl Clock for FaultClock {
        fn sample(&self) -> Result<Time, PolicyError> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            let fault = call == self.fault.load(Ordering::Relaxed);
            if fault && self.mode == 2 {
                return Err(PolicyError::Busy);
            }
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(if fault {
                    if self.mode == 0 {
                        100
                    } else {
                        1
                    }
                } else {
                    2
                }),
            })
        }
    }
    #[test]
    fn opening_shares_the_lookup_clock_and_late_errors_override_refusals() {
        with_blob(b"abc", BlobKind::Message, |_dir, pool, session| {
            let mut view = pool.capture(session).unwrap();
            let good = FaultClock::new(0);
            let input = view
                .open_blob_input(&Provider, &good, read_request(), ID, 3)
                .unwrap();
            drop(input);
            let samples = good.calls.load(Ordering::Relaxed);
            assert!(samples > 2);
            for mode in 0..3 {
                for remaining in 1..=2 {
                    let clock = FaultClock::new(mode);
                    clock.fault.store(samples - remaining, Ordering::Relaxed);
                    let expected = [
                        PolicyError::Deadline,
                        PolicyError::Invalid,
                        PolicyError::Busy,
                    ][mode as usize];
                    assert!(
                        matches!(view.open_blob_input(&Provider, &clock, read_request(), ID, 3),
                        Err(PinnedReadError::Policy(error)) if error == expected)
                    );
                    assert_eq!(clock.calls.load(Ordering::Relaxed), samples - remaining + 1);
                }
            }
            let clock = FaultClock::new(0);
            clock.fault.store(samples - 1, Ordering::Relaxed);
            assert!(matches!(
                view.open_blob_input(&Provider, &clock, read_request(), ID, 2),
                Err(PinnedReadError::Policy(PolicyError::Deadline))
            ));
        });
    }
    #[test]
    fn deadlines_regressions_and_source_errors_retire_body_steps() {
        for mode in 0..3 {
            for phase in 0..3 {
                for post in 0..2 {
                    with_blob(b"abc", BlobKind::Message, |_dir, pool, session| {
                        let clock = FaultClock::new(mode);
                        let expected = [
                            PolicyError::Deadline,
                            PolicyError::Invalid,
                            PolicyError::Busy,
                        ][mode as usize];
                        let mut view = pool.capture(session).unwrap();
                        let mut input = view
                            .open_blob_input(&Provider, &clock, read_request(), ID, 3)
                            .unwrap();
                        if phase == 0 {
                            clock.arm(post);
                            let mut out = [0; 3];
                            assert_eq!(input.read(&mut out), Err(expected));
                            if post == 1 {
                                assert_eq!(&out, b"abc");
                            }
                            let calls = clock.calls.load(Ordering::Relaxed);
                            assert_eq!(input.read(&mut out), Err(expected));
                            assert!(matches!(input.finish(), Err(e) if e == expected));
                            assert_eq!(clock.calls.load(Ordering::Relaxed), calls);
                        } else {
                            input.read(&mut [0; 3]).unwrap();
                            if phase == 1 {
                                clock.arm(post);
                                assert!(matches!(input.finish(), Err(e) if e == expected));
                            } else {
                                let mut body = input.finish().unwrap();
                                clock.arm(post);
                                let mut out = [0; 3];
                                assert_eq!(body.read_at(0, &mut out), Err(expected));
                                if post == 1 {
                                    assert_eq!(&out, b"abc");
                                }
                                let calls = clock.calls.load(Ordering::Relaxed);
                                assert_eq!(body.read_at(0, &mut out), Err(expected));
                                assert_eq!(clock.calls.load(Ordering::Relaxed), calls);
                            }
                        }
                        assert!(view
                            .open_blob_input(
                                &Provider,
                                &TestClock::new(u64::MAX),
                                read_request(),
                                ID,
                                3
                            )
                            .is_ok());
                    });
                }
            }
        }
    }
}
