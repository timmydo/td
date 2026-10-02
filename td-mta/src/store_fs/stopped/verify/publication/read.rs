//! Borrowed query scopes over one retained, committed prefix.
use super::super::super::super::{
    OverlayInputError, SelectionError, SelectionScratch, ValidationError, ValidationLimits,
    ValidationReadRequest,
};
use super::{CommittedView, VerifyClock};
use crate::{
    format::table::MAX_RECORD_BYTES,
    frame_changes, overlay,
    ports::{Clock, Crypto, Error as PolicyError, ReadView},
};
use std::sync::atomic::AtomicU64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinnedReadRequest {
    pub overlay_bytes: u64,
    pub files: ValidationLimits,
    pub query: ValidationReadRequest,
}
/// Exclusive caller partitions; no allocation or scratch owner is created here.
pub struct PinnedReadScratch<'a> {
    pub selection: &'a mut SelectionScratch,
    pub frames: &'a mut [u8],
    pub cells: &'a mut [overlay::Cell],
    pub record: &'a mut [u8; MAX_RECORD_BYTES],
    pub changes: &'a mut [frame_changes::Cell],
}
#[derive(Debug)]
pub enum PinnedReadError {
    Policy(PolicyError),
    Selection(SelectionError),
    Overlay(OverlayInputError),
    Files(ValidationError),
}
impl std::fmt::Display for PinnedReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("committed-prefix read failed")
    }
}
impl std::error::Error for PinnedReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Policy(e) => Some(e),
            Self::Selection(e) => Some(e),
            Self::Overlay(e) => Some(e),
            Self::Files(e) => Some(e),
        }
    }
}
fn checked<T>(
    clock: &VerifyClock<'_>,
    work: impl FnOnce() -> Result<T, PinnedReadError>,
) -> Result<T, PinnedReadError> {
    clock.sample().map_err(PinnedReadError::Policy)?;
    let result = work();
    clock.sample().map_err(PinnedReadError::Policy)?;
    result
}
impl CommittedView<'_, '_, '_> {
    /// One query scope per pin at a time. Tables/history stay immutable and active
    /// reads stop at this pin's captured offset while the serialized writer appends.
    /// Admit the full bounded preparation plus each query's work before calling.
    /// Scratch may be overwritten on any result; errors do not retire the writer.
    ///
    /// ```compile_fail
    /// use td_mta::{ports::Clock, store_fs::{CommittedView, PinnedReadRequest,
    ///     PinnedReadScratch}};
    /// fn escape(pin: &mut CommittedView<'_, '_, '_>, clock: &dyn Clock,
    ///     request: PinnedReadRequest, scratch: PinnedReadScratch<'_>) {
    ///     let reader = pin.with_read_view(&td_crypto::Provider, clock, request,
    ///         scratch, |reader| Ok(reader)).unwrap();
    ///     let _ = reader.identity();
    /// }
    /// ```
    pub fn with_read_view<C: Crypto, R>(
        &mut self,
        crypto: &C,
        clock: &dyn Clock,
        request: PinnedReadRequest,
        scratch: PinnedReadScratch<'_>,
        run: impl FnOnce(&mut dyn ReadView) -> Result<R, PolicyError>,
    ) -> Result<R, PinnedReadError>
    where
        C::Sha256: Sync,
    {
        let clock = VerifyClock {
            source: clock,
            deadline: request.query.deadline,
            last: AtomicU64::new(0),
        };
        let store = &self.session.store.store;
        let selection = checked(&clock, || {
            store
                .load_selection(crypto, self.identity.account, scratch.selection)
                .map_err(PinnedReadError::Selection)
        })?;
        if selection.current() != self.session.store.current {
            return Err(PinnedReadError::Policy(PolicyError::Corrupt));
        }
        let active = checked(&clock, || {
            store
                .load_active_overlay(
                    crypto,
                    selection,
                    self.identity,
                    request.overlay_bytes,
                    scratch.frames,
                    scratch.cells,
                )
                .map_err(PinnedReadError::Overlay)
        })?;
        let mut files = checked(&clock, || {
            store
                .validate_files(
                    crypto,
                    selection,
                    &active,
                    scratch.record,
                    scratch.changes,
                    request.files,
                )
                .map_err(PinnedReadError::Files)
        })?;
        while !files.is_complete() {
            checked(&clock, || files.advance().map_err(PinnedReadError::Files))?;
        }
        let (files, record, changes) =
            checked(&clock, || files.finish().map_err(PinnedReadError::Files))?;
        let mut view = checked(&clock, || {
            files
                .read_view(crypto, &clock, request.query, record, changes)
                .map_err(PinnedReadError::Policy)
        })?;
        checked(&clock, || {
            let result = run(&mut view);
            // A callback cannot turn a retired reader into a successful scope.
            view.check_deadline().map_err(PinnedReadError::Policy)?;
            result.map_err(PinnedReadError::Policy)
        })
    }
}
