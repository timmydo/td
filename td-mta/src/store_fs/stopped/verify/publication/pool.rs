//! Startup-owned read scratch, moved between a fixed slot and one borrowed pin.
use super::{
    CommittedView, JournalError, JournalSession, PinnedReadError, PinnedReadRequest,
    PinnedReadScratch,
};
use crate::{
    frame_changes,
    limits::ResourcePlan,
    overlay,
    ports::{Clock, Crypto, Error as PolicyError, ReadView, ViewIdentity},
};
use std::sync::{Mutex, TryLockError};

#[path = "pool/blob.rs"]
mod blob;
pub use blob::{PinnedBlob, PinnedBlobInput};

#[derive(Debug)]
pub enum ReadPoolError {
    InvalidBacking,
    Capacity,
    Busy,
    Poisoned,
    Journal(JournalError),
}
impl std::fmt::Display for ReadPoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidBacking => "read pool backing does not match admission",
            Self::Capacity => "read pool is full",
            Self::Busy => "read pool slot is busy",
            Self::Poisoned => "read pool slot is poisoned",
            Self::Journal(_) => "read pool could not capture a journal pin",
        })
    }
}
impl std::error::Error for ReadPoolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Journal(e) => Some(e),
            _ => None,
        }
    }
}
/// Construct and touch caller backing before admission; no heap allocation is added.
pub struct ReadScratchSlot<'a> {
    scratch: Mutex<Option<PinnedReadScratch<'a>>>,
}
impl<'a> ReadScratchSlot<'a> {
    pub fn new(plan: &ResourcePlan, scratch: PinnedReadScratch<'a>) -> Result<Self, ReadPoolError> {
        let limits = plan.limits();
        if scratch.frames.len() != limits.journal_bytes
            || scratch.cells.len() != limits.journal_operations
            || scratch.changes.len() != limits.frame_operations
            || std::mem::size_of_val(scratch.selection) > 5 * 1024
            || std::mem::size_of_val(scratch.record) > 68 * 1024
            || std::mem::size_of::<Self>() > 256
        {
            return Err(ReadPoolError::InvalidBacking);
        }
        *scratch.selection = super::SelectionScratch::new();
        scratch.frames.fill(0);
        scratch.cells.fill(overlay::Cell::EMPTY);
        scratch.record.fill(0);
        scratch.changes.fill(frame_changes::Cell::EMPTY);
        Ok(Self {
            scratch: Mutex::new(Some(scratch)),
        })
    }
}
/// Exclusive slot-array ownership prevents another pool from admitting these slots.
///
/// ```compile_fail
/// use td_mta::{limits::ResourcePlan, store_fs::{ReadScratchPool, ReadScratchSlot,
///     JournalSession}};
/// fn rebuild(plan: &ResourcePlan, slots: &mut [ReadScratchSlot<'_>],
///     session: &JournalSession<'_, '_>) {
///     let pool = ReadScratchPool::new(plan, slots).unwrap();
///     let reader = pool.capture(session).unwrap();
///     let rebuilt = ReadScratchPool::new(plan, slots).unwrap();
///     let _ = reader.identity();
///     let _ = rebuilt.capacity();
/// }
/// ```
///
/// ```compile_fail
/// use td_mta::{limits::ResourcePlan, store_fs::{ReadScratchPool, ReadScratchSlot,
///     JournalSession}};
/// fn escape(plan: &ResourcePlan, slots: &mut [ReadScratchSlot<'_>],
///     session: &JournalSession<'_, '_>) {
///     let reader = {
///         let pool = ReadScratchPool::new(plan, slots).unwrap();
///         pool.capture(session).unwrap()
///     };
///     let _ = reader.identity();
/// }
/// ```
pub struct ReadScratchPool<'p, 's> {
    slots: &'p mut [ReadScratchSlot<'s>],
}
impl<'p, 's> ReadScratchPool<'p, 's> {
    pub fn new(
        plan: &ResourcePlan,
        slots: &'p mut [ReadScratchSlot<'s>],
    ) -> Result<Self, ReadPoolError> {
        if slots.len() != plan.limits().storage_views
            || std::mem::size_of::<Self>() > 64
            || std::mem::size_of::<PooledRead<'_, '_, '_, '_, '_>>() > 512
        {
            return Err(ReadPoolError::InvalidBacking);
        }
        for slot in slots.iter_mut() {
            let state = slot
                .scratch
                .get_mut()
                .map_err(|_| ReadPoolError::Poisoned)?;
            if state.is_none() {
                return Err(ReadPoolError::InvalidBacking);
            }
        }
        Ok(Self { slots })
    }
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }
    fn acquire(&self) -> Result<ScratchLease<'_, 's>, ReadPoolError> {
        let mut busy = false;
        for slot in self.slots.iter() {
            let mut state = match slot.scratch.try_lock() {
                Ok(state) => state,
                Err(TryLockError::WouldBlock) => {
                    busy = true;
                    continue;
                }
                Err(TryLockError::Poisoned(_)) => return Err(ReadPoolError::Poisoned),
            };
            if let Some(scratch) = state.take() {
                return Ok(ScratchLease {
                    slot,
                    scratch: Some(scratch),
                });
            }
        }
        Err(if busy {
            ReadPoolError::Busy
        } else {
            ReadPoolError::Capacity
        })
    }
    /// Acquire scratch before capturing identity, releasing it on any capture refusal.
    /// No slot lock is held while acquiring the publication lock or doing read I/O.
    pub fn capture<'v, 'r, 'l>(
        &self,
        session: &'v JournalSession<'r, 'l>,
    ) -> Result<PooledRead<'v, 'r, 'l, '_, 's>, ReadPoolError> {
        if self.capacity() != usize::from(session.views) {
            return Err(ReadPoolError::InvalidBacking);
        }
        let scratch = self.acquire()?;
        let pin = session.capture().map_err(ReadPoolError::Journal)?;
        Ok(PooledRead { pin, scratch })
    }
}
struct ScratchLease<'p, 's> {
    slot: &'p ReadScratchSlot<'s>,
    scratch: Option<PinnedReadScratch<'s>>,
}
impl Drop for ScratchLease<'_, '_> {
    fn drop(&mut self) {
        let Some(scratch) = self.scratch.take() else {
            return;
        };
        // Only this lease can return to its empty slot. Preserve any poison flag.
        let mut state = match self.slot.scratch.lock() {
            Ok(state) => state,
            Err(error) => error.into_inner(),
        };
        *state = Some(scratch);
    }
}
/// Owns both a committed pin and one preallocated scratch lease; neither is clonable.
pub struct PooledRead<'v, 'r, 'l, 'p, 's> {
    pin: CommittedView<'v, 'r, 'l>,
    scratch: ScratchLease<'p, 's>,
}
impl PooledRead<'_, '_, '_, '_, '_> {
    pub const fn identity(&self) -> ViewIdentity {
        self.pin.identity()
    }
    pub fn with_read_view<C: Crypto, R>(
        &mut self,
        crypto: &C,
        clock: &dyn Clock,
        request: PinnedReadRequest,
        run: impl FnOnce(&mut dyn ReadView) -> Result<R, PolicyError>,
    ) -> Result<R, PinnedReadError>
    where
        C::Sha256: Sync,
    {
        let scratch = self
            .scratch
            .scratch
            .as_mut()
            .ok_or(PinnedReadError::Policy(PolicyError::Invalid))?;
        self.pin.with_read_view(
            crypto,
            clock,
            request,
            PinnedReadScratch {
                selection: scratch.selection,
                frames: scratch.frames,
                cells: scratch.cells,
                record: scratch.record,
                changes: scratch.changes,
            },
            run,
        )
    }
}

#[cfg(test)]
pub use tests::probe as probe_read_pool;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::super::super::tests::owned_fixture;
    use super::super::tests::{
        budget, deadline, frame, inspect_reader, read_request, start, with_ledger, CommitFixture,
        TestClock,
    };
    use super::super::SelectionScratch;
    use super::*;
    use crate::{format::table::MAX_RECORD_BYTES, limits::Limits};
    use td_crypto::Provider;

    pub(super) struct Backing {
        selection: SelectionScratch,
        frames: Vec<u8>,
        cells: Vec<overlay::Cell>,
        record: Box<[u8; MAX_RECORD_BYTES]>,
        changes: Vec<frame_changes::Cell>,
    }
    impl Backing {
        pub(super) fn new(plan: &ResourcePlan) -> Self {
            Self {
                selection: SelectionScratch::new(),
                frames: vec![0xa5; plan.limits().journal_bytes],
                cells: vec![overlay::Cell::EMPTY; plan.limits().journal_operations],
                record: vec![0xa5; MAX_RECORD_BYTES]
                    .into_boxed_slice()
                    .try_into()
                    .unwrap(),
                changes: vec![frame_changes::Cell::EMPTY; plan.limits().frame_operations],
            }
        }
        pub(super) fn borrow(&mut self) -> PinnedReadScratch<'_> {
            PinnedReadScratch {
                selection: &mut self.selection,
                frames: &mut self.frames,
                cells: &mut self.cells,
                record: &mut self.record,
                changes: &mut self.changes,
            }
        }
    }
    #[derive(Clone, Copy)]
    enum ProbeCase {
        Queries,
        Append,
        Deadline,
        WorkLimit,
        Full,
        SlotBusy,
        JournalBusy,
        JournalFull,
    }
    /// Allocate/touch full backing and verify the fixture before measuring pool operations.
    pub fn probe(mut snapshot: impl FnMut()) {
        use super::super::super::super::super::tests::Fixture;
        use super::super::super::tests::owned_fixture_with;
        for maximum in [false, true] {
            for case in [
                ProbeCase::Queries,
                ProbeCase::Append,
                ProbeCase::Deadline,
                ProbeCase::WorkLimit,
                ProbeCase::Full,
                ProbeCase::SlotBusy,
                ProbeCase::JournalBusy,
                ProbeCase::JournalFull,
            ] {
                let fixture = if maximum {
                    Fixture::maximum_root()
                } else {
                    Fixture::new()
                };
                let (_dir, verified, mut startup) = owned_fixture_with(fixture);
                let plan = Limits::default().plan().unwrap();
                let mut a = Backing::new(&plan);
                let mut b = Backing::new(&plan);
                let mut slots = [
                    ReadScratchSlot::new(&plan, a.borrow()).unwrap(),
                    ReadScratchSlot::new(&plan, b.borrow()).unwrap(),
                ];
                let pool = ReadScratchPool::new(&plan, &mut slots).unwrap();
                let clock = TestClock::new(u64::MAX);
                let early = TestClock::new(0);
                let next_frame = frame(3);
                with_ledger(|ledger| {
                    verified
                        .with_journal(
                            &Provider,
                            &clock,
                            ledger,
                            start(),
                            startup.journal(),
                            |session, _| {
                                snapshot();
                                match case {
                                    ProbeCase::Full => {
                                        let a = pool.capture(session).unwrap();
                                        let b = pool.capture(session).unwrap();
                                        assert!(matches!(
                                            pool.capture(session),
                                            Err(ReadPoolError::Capacity)
                                        ));
                                        drop(a);
                                        drop(b);
                                    }
                                    ProbeCase::SlotBusy => {
                                        let a = pool.slots[0].scratch.lock().unwrap();
                                        let b = pool.slots[1].scratch.lock().unwrap();
                                        assert!(matches!(
                                            pool.capture(session),
                                            Err(ReadPoolError::Busy)
                                        ));
                                        drop(a);
                                        drop(b);
                                    }
                                    ProbeCase::JournalBusy => {
                                        let guard = session.published.lock().unwrap();
                                        assert!(matches!(
                                            pool.capture(session),
                                            Err(ReadPoolError::Journal(JournalError::Policy(
                                                PolicyError::Busy
                                            )))
                                        ));
                                        drop(guard);
                                    }
                                    ProbeCase::JournalFull => {
                                        let a = session.capture().unwrap();
                                        let b = session.capture().unwrap();
                                        assert!(matches!(
                                            pool.capture(session),
                                            Err(ReadPoolError::Journal(JournalError::Policy(
                                                PolicyError::Capacity
                                            )))
                                        ));
                                        drop(a);
                                        drop(b);
                                    }
                                    _ => (),
                                }
                                let mut old = pool.capture(session).unwrap();
                                let mut request = read_request();
                                if matches!(case, ProbeCase::WorkLimit) {
                                    request.query.limits.steps = 0;
                                }
                                let source: &dyn Clock = if matches!(case, ProbeCase::Deadline) {
                                    &early
                                } else {
                                    &clock
                                };
                                let result =
                                    old.with_read_view(&Provider, source, request, |view| {
                                        if matches!(case, ProbeCase::Append) {
                                            session
                                                .commit(
                                                    &Provider,
                                                    &clock,
                                                    deadline(),
                                                    &next_frame,
                                                    budget(),
                                                )
                                                .unwrap();
                                        }
                                        inspect_reader(view)
                                    });
                                match case {
                                    ProbeCase::Deadline => assert!(matches!(
                                        result,
                                        Err(PinnedReadError::Policy(PolicyError::Deadline))
                                    )),
                                    ProbeCase::WorkLimit => assert!(matches!(
                                        result,
                                        Err(PinnedReadError::Policy(PolicyError::Capacity))
                                    )),
                                    _ => assert_eq!(result.unwrap(), 2),
                                }
                                assert_eq!(
                                    old.with_read_view(
                                        &Provider,
                                        &clock,
                                        read_request(),
                                        inspect_reader
                                    )
                                    .unwrap(),
                                    2
                                );
                                let mut new = pool.capture(session).unwrap();
                                let expected = if matches!(case, ProbeCase::Append) {
                                    3
                                } else {
                                    2
                                };
                                assert_eq!(
                                    new.with_read_view(
                                        &Provider,
                                        &clock,
                                        read_request(),
                                        inspect_reader
                                    )
                                    .unwrap(),
                                    expected
                                );
                                drop(new);
                                drop(old);
                                snapshot();
                            },
                        )
                        .unwrap();
                });
            }
        }
    }
    #[test]
    fn backing_is_exact_and_refusal_precedes_touching() {
        let plan = Limits::default().plan().unwrap();
        for mode in 0..6 {
            let mut backing = Backing::new(&plan);
            match mode {
                0 => {
                    backing.frames.pop();
                }
                1 => backing.frames.push(0xa5),
                2 => {
                    backing.cells.pop();
                }
                3 => backing.cells.push(overlay::Cell::EMPTY),
                4 => {
                    backing.changes.pop();
                }
                _ => backing.changes.push(frame_changes::Cell::EMPTY),
            }
            assert!(matches!(
                ReadScratchSlot::new(&plan, backing.borrow()),
                Err(ReadPoolError::InvalidBacking)
            ));
            assert!(backing.frames.iter().all(|b| *b == 0xa5));
            assert!(backing.record.iter().all(|b| *b == 0xa5));
        }
        let mut backing = Backing::new(&plan);
        let mut slots = [ReadScratchSlot::new(&plan, backing.borrow()).unwrap()];
        assert!(matches!(
            ReadScratchPool::new(&plan, &mut slots),
            Err(ReadPoolError::InvalidBacking)
        ));
        assert!(backing.frames.iter().all(|b| *b == 0));
        assert!(backing.record.iter().all(|b| *b == 0));
    }
    #[test]
    fn pool_returns_unique_backing_and_preserves_poison() {
        let plan = Limits {
            storage_views: 1,
            ..Limits::default()
        }
        .plan()
        .unwrap();
        let mut backing = Backing::new(&plan);
        let expected = backing.frames.as_ptr();
        let mut slots = [ReadScratchSlot::new(&plan, backing.borrow()).unwrap()];
        let pool = ReadScratchPool::new(&plan, &mut slots).unwrap();
        assert_eq!(pool.capacity(), 1);
        let mut lease = pool.acquire().unwrap();
        assert_eq!(lease.scratch.as_ref().unwrap().frames.as_ptr(), expected);
        lease.scratch.as_mut().unwrap().frames[0] = 7;
        assert!(matches!(pool.acquire(), Err(ReadPoolError::Capacity)));
        drop(lease);
        let lease = pool.acquire().unwrap();
        assert_eq!(lease.scratch.as_ref().unwrap().frames[0], 7);
        drop(lease);
        let guard = pool.slots[0].scratch.lock().unwrap();
        assert!(matches!(pool.acquire(), Err(ReadPoolError::Busy)));
        drop(guard);
        let lease = pool.acquire().unwrap();
        assert!(std::panic::catch_unwind(|| {
            let _guard = pool.slots[0].scratch.lock().unwrap();
            panic!("poison fixture");
        })
        .is_err());
        drop(lease);
        assert!(pool.slots[0].scratch.is_poisoned());
        assert!(pool.slots[0]
            .scratch
            .lock()
            .err()
            .unwrap()
            .into_inner()
            .is_some());
        assert!(matches!(pool.acquire(), Err(ReadPoolError::Poisoned)));
        assert!(matches!(
            ReadScratchPool::new(&plan, &mut slots),
            Err(ReadPoolError::Poisoned)
        ));
    }
    #[test]
    fn busy_slots_are_skipped_and_session_count_must_match() {
        with_ledger(|ledger| {
            let plan = Limits::default().plan().unwrap();
            let mut a = Backing::new(&plan);
            let mut b = Backing::new(&plan);
            let mut slots = [
                ReadScratchSlot::new(&plan, a.borrow()).unwrap(),
                ReadScratchSlot::new(&plan, b.borrow()).unwrap(),
            ];
            let pool = ReadScratchPool::new(&plan, &mut slots).unwrap();
            let guard = pool.slots[0].scratch.lock().unwrap();
            let lease = pool.acquire().unwrap();
            assert!(matches!(pool.acquire(), Err(ReadPoolError::Busy)));
            drop(lease);
            drop(guard);
            let (_dir, verified, mut startup) = owned_fixture();
            let clock = TestClock::new(u64::MAX);
            let mut request = start();
            request.views = 1;
            verified
                .with_journal(
                    &Provider,
                    &clock,
                    ledger,
                    request,
                    startup.journal(),
                    |session, _| {
                        assert!(matches!(
                            pool.capture(session),
                            Err(ReadPoolError::InvalidBacking)
                        ));
                        assert_eq!(session.published.lock().unwrap().readers, 0);
                    },
                )
                .unwrap();
        });
    }
    #[test]
    fn pooled_queries_reuse_scratch_across_append_refusal_and_thread_handoff() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<PooledRead<'_, '_, '_, '_, '_>>();
        with_ledger(|ledger| {
            let plan = Limits::default().plan().unwrap();
            let mut a = Backing::new(&plan);
            let mut b = Backing::new(&plan);
            let mut slots = [
                ReadScratchSlot::new(&plan, a.borrow()).unwrap(),
                ReadScratchSlot::new(&plan, b.borrow()).unwrap(),
            ];
            let pool = ReadScratchPool::new(&plan, &mut slots).unwrap();
            let (_dir, verified, mut startup) = owned_fixture();
            let clock = TestClock::new(u64::MAX);
            verified
                .with_journal(
                    &Provider,
                    &clock,
                    ledger,
                    start(),
                    startup.journal(),
                    |session, _| {
                        let mut old = pool.capture(session).unwrap();
                        assert_eq!(old.identity().committed_sequence.number(), 2);
                        assert_eq!(
                            old.with_read_view(&Provider, &clock, read_request(), |view| {
                                session
                                    .commit_fixture(
                                        &Provider,
                                        &clock,
                                        deadline(),
                                        &frame(3),
                                        budget(),
                                    )
                                    .unwrap();
                                let other = pool.capture(session).unwrap();
                                assert_eq!(other.identity().committed_sequence.number(), 3);
                                assert!(matches!(
                                    pool.capture(session),
                                    Err(ReadPoolError::Capacity)
                                ));
                                inspect_reader(view)
                            })
                            .unwrap(),
                            2
                        );
                        let mut request = read_request();
                        request.overlay_bytes = 0;
                        assert!(matches!(
                            old.with_read_view(&Provider, &clock, request, inspect_reader),
                            Err(PinnedReadError::Overlay(_))
                        ));
                        std::thread::scope(|scope| {
                            scope
                                .spawn(move || {
                                    assert_eq!(
                                        old.with_read_view(
                                            &Provider,
                                            &TestClock::new(u64::MAX),
                                            read_request(),
                                            inspect_reader
                                        )
                                        .unwrap(),
                                        2
                                    );
                                })
                                .join()
                                .unwrap();
                        });
                        let mut new = pool.capture(session).unwrap();
                        let other = pool.capture(session).unwrap();
                        assert_eq!(
                            new.with_read_view(
                                &Provider,
                                &TestClock::new(u64::MAX),
                                read_request(),
                                inspect_reader
                            )
                            .unwrap(),
                            3
                        );
                        drop(other);
                        drop(new);
                        assert_eq!(session.published.lock().unwrap().readers, 0);
                    },
                )
                .unwrap();
        });
    }
    #[test]
    fn forgotten_reader_prevents_reconstruction_of_its_slot_array() {
        with_ledger(|ledger| {
            let plan = Limits::default().plan().unwrap();
            let mut a = Backing::new(&plan);
            let mut b = Backing::new(&plan);
            let mut slots = [
                ReadScratchSlot::new(&plan, a.borrow()).unwrap(),
                ReadScratchSlot::new(&plan, b.borrow()).unwrap(),
            ];
            let (_dir, verified, mut startup) = owned_fixture();
            let clock = TestClock::new(u64::MAX);
            verified
                .with_journal(
                    &Provider,
                    &clock,
                    ledger,
                    start(),
                    startup.journal(),
                    |session, _| {
                        let pool = ReadScratchPool::new(&plan, &mut slots).unwrap();
                        std::mem::forget(pool.capture(session).unwrap());
                        assert_eq!(session.published.lock().unwrap().readers, 1);
                        assert!(matches!(
                            ReadScratchPool::new(&plan, &mut slots),
                            Err(ReadPoolError::InvalidBacking)
                        ));
                    },
                )
                .unwrap();
        });
    }
    #[test]
    fn capture_failure_and_callback_unwind_return_both_resources() {
        with_ledger(|ledger| {
            let plan = Limits::default().plan().unwrap();
            let mut a = Backing::new(&plan);
            let mut b = Backing::new(&plan);
            let mut slots = [
                ReadScratchSlot::new(&plan, a.borrow()).unwrap(),
                ReadScratchSlot::new(&plan, b.borrow()).unwrap(),
            ];
            let pool = ReadScratchPool::new(&plan, &mut slots).unwrap();
            let (_dir, verified, mut startup) = owned_fixture();
            let clock = TestClock::new(u64::MAX);
            verified
                .with_journal(
                    &Provider,
                    &clock,
                    ledger,
                    start(),
                    startup.journal(),
                    |session, _| {
                        let guard = session.published.lock().unwrap();
                        assert!(matches!(
                            pool.capture(session),
                            Err(ReadPoolError::Journal(JournalError::Policy(
                                PolicyError::Busy
                            )))
                        ));
                        drop(guard);
                        let a = session.capture().unwrap();
                        let b = session.capture().unwrap();
                        assert!(matches!(
                            pool.capture(session),
                            Err(ReadPoolError::Journal(JournalError::Policy(
                                PolicyError::Capacity
                            )))
                        ));
                        drop(a);
                        drop(b);
                        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let mut reader = pool.capture(session).unwrap();
                            reader
                                .with_read_view(
                                    &Provider,
                                    &clock,
                                    read_request(),
                                    |_| -> Result<(), PolicyError> {
                                        panic!("reader callback fixture");
                                    },
                                )
                                .unwrap();
                        }))
                        .is_err());
                        let a = pool.capture(session).unwrap();
                        let b = pool.capture(session).unwrap();
                        drop(a);
                        drop(b);
                        assert_eq!(session.published.lock().unwrap().readers, 0);
                        assert!(pool.slots.iter().all(|slot| !slot.scratch.is_poisoned()));
                    },
                )
                .unwrap();
        });
    }
}
