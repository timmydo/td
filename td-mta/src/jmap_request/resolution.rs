//! Metered result-reference expansion inside one pinned response file.
use super::pointer::{self, Piece, Walker};
use crate::{ports, store_fs::ResponseRetention};
use std::{fmt, ops::Range};
use td_json::scan;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Pointer(pointer::Error),
    Store(ports::Error),
    State,
}
impl fmt::Display for Error {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pointer(error) => error.fmt(out),
            Self::Store(error) => error.fmt(out),
            Self::State => out.write_str("invalid retained resolution state"),
        }
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy)]
enum Phase {
    Walk,
    Copy { offset: u64, left: u64 },
    Done,
}

/// Exclusively borrows one retention owner across all reads, CPU turns, copies
/// and publication. The caller binds the source range to its earlier response
/// index; this constructor verifies the immutable published extent, not which
/// invocation produced it. No raw descriptor or path is exposed.
///
/// max_output excludes the caller's mandatory response reserve; every append
/// checks this per-expansion ceiling before writing. A ceiling refusal is
/// recoverable if the original retention owner remains healthy.
///
/// Results append as a separate JSON value in the same private file. The final
/// HTTP encoder must use indexed ranges, never send this mixed-content file as
/// a whole Response. Failed expansion rolls back only its unpublished tail;
/// earlier method results and successful expansions remain immutable. Terminal
/// work, time or I/O failure can prevent truncation; whole-file cleanup then
/// removes the still-unpublished tail.
///
/// Every poll and finish observes the retention owner's original deadline and
/// foreground record budget. Reads and copies also charge its I/O/output work
/// and disk cap. The dispatcher separately enforces its execution deadline.
/// Drop cancels unfinished output; any rollback fault remains sticky in the
/// borrowed retention owner. Explicit cancel reports that fault to the caller.
pub struct Resolution<'io, 'path, 'c, 's, 'sr, 'a> {
    retention: &'io mut ResponseRetention<'c, 's, 'sr, 'a>,
    walker: Walker<'path>,
    source: Range<u64>,
    read: u64,
    input: [u8; scan::TURN_BYTES],
    used: usize,
    filled: usize,
    output_start: u64,
    max_output: u64,
    phase: Phase,
    array: bool,
    first: bool,
    settled: bool,
    failure: Option<Error>,
}
impl fmt::Debug for Resolution<'_, '_, '_, '_, '_, '_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("Resolution { .. }")
    }
}
impl<'io, 'path, 'c, 's, 'sr, 'a> Resolution<'io, 'path, 'c, 's, 'sr, 'a> {
    pub fn new(
        retention: &'io mut ResponseRetention<'c, 's, 'sr, 'a>,
        source: Range<u64>,
        path: &'path str,
        max_output: u64,
    ) -> Result<Self, Error> {
        if source.start >= source.end
            || source.end > retention.published_len()
            || retention.len() != retention.published_len()
        {
            return Err(Error::State);
        }
        retention.admit_work_turn().map_err(Error::Store)?;
        let walker = Walker::new(path, source.end - source.start, scan::MAX_DEPTH)
            .map_err(Error::Pointer)?;
        let output_start = retention.len();
        Ok(Self {
            retention,
            walker,
            read: source.start,
            source,
            input: [0; scan::TURN_BYTES],
            used: 0,
            filled: 0,
            output_start,
            max_output,
            phase: Phase::Walk,
            array: false,
            first: true,
            settled: false,
            failure: None,
        })
    }
    /// Check an enclosing execution phase through the same original clock and
    /// meter. A false result leaves the tail cancellable under the outer budget.
    pub fn admit_before(&mut self, phase: ports::Deadline) -> Result<bool, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.retention.admit_before(phase).map_err(Error::Store)
    }
    /// One bounded turn. Scratch is used for at most TURN_BYTES of value copy;
    /// it is not retained and may be reused between polls. Empty scratch is a
    /// terminal State error in every phase; like other failed polls it spends
    /// admission and attempts tail rollback. The retention borrow survives.
    pub fn poll(&mut self, scratch: &mut [u8]) -> Result<bool, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.inner(scratch);
        if let Err(error) = result {
            let error = match self.rollback() {
                Ok(()) => error,
                Err(cleanup) => cleanup,
            };
            self.failure = Some(error);
            return Err(error);
        }
        result
    }
    fn inner(&mut self, scratch: &mut [u8]) -> Result<bool, Error> {
        self.retention.admit_work_turn().map_err(Error::Store)?;
        if scratch.is_empty() {
            return Err(Error::State);
        }
        match self.phase {
            Phase::Done => return Ok(true),
            Phase::Copy { offset, left } => {
                let amount = usize::try_from(left.min(scan::TURN_BYTES as u64))
                    .map_err(|_| Error::State)?
                    .min(scratch.len());
                let chunk = scratch.get_mut(..amount).ok_or(Error::State)?;
                self.retention
                    .read_at(offset, chunk)
                    .map_err(Error::Store)?;
                self.append(chunk)?;
                let left = left.checked_sub(amount as u64).ok_or(Error::State)?;
                self.phase = if left == 0 {
                    Phase::Walk
                } else {
                    Phase::Copy {
                        offset: offset.checked_add(amount as u64).ok_or(Error::State)?,
                        left,
                    }
                };
                return Ok(false);
            }
            Phase::Walk => (),
        }
        if self.used == self.filled && self.read < self.source.end {
            let amount =
                usize::try_from((self.source.end - self.read).min(scan::TURN_BYTES as u64))
                    .map_err(|_| Error::State)?;
            self.retention
                .read_at(self.read, self.input.get_mut(..amount).ok_or(Error::State)?)
                .map_err(Error::Store)?;
            self.read = self.read.checked_add(amount as u64).ok_or(Error::State)?;
            self.used = 0;
            self.filled = amount;
        }
        let next = self
            .walker
            .poll(
                self.input.get(self.used..self.filled).ok_or(Error::State)?,
                self.read == self.source.end,
            )
            .map_err(Error::Pointer)?;
        self.used = self.used.checked_add(next.consumed).ok_or(Error::State)?;
        match next.piece {
            Some(Piece::ArrayStart) => {
                if self.array || !self.first {
                    return Err(Error::State);
                }
                self.array = true;
                self.append(b"[")?;
            }
            Some(Piece::ArrayEnd) => {
                if !self.array {
                    return Err(Error::State);
                }
                self.append(b"]")?;
            }
            Some(Piece::Value { offset, length }) => {
                let offset = self.source.start.checked_add(offset).ok_or(Error::State)?;
                if length == 0 || offset.checked_add(length).ok_or(Error::State)? > self.source.end
                {
                    return Err(Error::State);
                }
                if !self.first {
                    if !self.array {
                        return Err(Error::State);
                    }
                    self.append(b",")?;
                }
                self.first = false;
                self.phase = Phase::Copy {
                    offset,
                    left: length,
                };
            }
            None => (),
        }
        if next.complete {
            if !matches!(self.phase, Phase::Walk)
                || self.used != self.filled
                || self.read != self.source.end
                || self.retention.len() == self.output_start
            {
                return Err(Error::State);
            }
            self.phase = Phase::Done;
            return Ok(true);
        }
        Ok(false)
    }
    fn append(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let length = self
            .retention
            .len()
            .checked_sub(self.output_start)
            .and_then(|used| used.checked_add(bytes.len() as u64))
            .ok_or(Error::State)?;
        if length > self.max_output {
            return Err(Error::Store(ports::Error::Capacity));
        }
        self.retention.append(bytes).map_err(Error::Store)
    }
    /// Freshly admit and publish the completed value before returning its range.
    /// Incomplete finish cancels its tail and reports State, or a rollback fault.
    pub fn finish(mut self) -> Result<Range<u64>, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !matches!(self.phase, Phase::Done) {
            self.rollback()?;
            return Err(Error::State);
        }
        self.retention.admit_work_turn().map_err(Error::Store)?;
        self.retention.publish().map_err(Error::Store)?;
        self.settled = true;
        Ok(self.output_start..self.retention.len())
    }
    /// Report any previous failure, including a failed rollback attempt.
    pub fn cancel(mut self) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.rollback()
    }
    fn rollback(&mut self) -> Result<(), Error> {
        if self.settled {
            return Ok(());
        }
        self.settled = true;
        self.retention.rollback_tail().map_err(Error::Store)
    }
}
impl Drop for Resolution<'_, '_, '_, '_, '_, '_> {
    fn drop(&mut self) {
        let _ = self.rollback();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
pub(super) mod tests {
    use super::*;
    use crate::{
        admission::{logical::Cell as LeaseCell, DiskLimits, ViewMode, WorkLimits},
        limits::Limits,
        ownership::SlotState,
        ports::{Clock, Deadline, Tick, Time},
        store_fs::{tests::Fixture, RequestSpool, ResponseCoordinator},
    };
    use std::{
        fs,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
    };
    pub(crate) struct Timer(pub(crate) AtomicU64, AtomicU64);
    impl Timer {
        pub(crate) fn advance_until(&self, tick: u64) {
            self.1.store(tick, Ordering::SeqCst);
        }
    }
    impl Clock for Timer {
        fn sample(&self) -> Result<Time, ports::Error> {
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(
                    self.0
                        .try_update(Ordering::SeqCst, Ordering::SeqCst, |now| {
                            (now < self.1.load(Ordering::SeqCst)).then(|| now + 1)
                        })
                        .unwrap_or_else(|now| now),
                ),
            })
        }
    }
    fn deadline() -> Deadline {
        Deadline::after(Tick(0), 100).unwrap()
    }
    pub(crate) fn run(test: impl FnOnce(&ResponseCoordinator<'_, '_>, &Fixture, &Timer)) {
        let files = Fixture::new();
        let mut file_root = files.locked();
        let clock = Arc::new(Timer(AtomicU64::new(1), AtomicU64::new(0)));
        let spool = RequestSpool::open(&mut file_root, clock.clone(), deadline()).unwrap();
        let resources = Limits {
            json_bytes: 64,
            json_methods: 1,
            ..Limits::default()
        }
        .plan()
        .unwrap();
        let plan = DiskLimits {
            response_bytes: 72000,
            response_total_bytes: 144000,
            ..DiskLimits::default()
        }
        .plan(
            &resources,
            WorkLimits::default(),
            ViewMode::OnlineBackground,
        )
        .unwrap();
        let mut states = [const { SlotState::EMPTY }; 4];
        let mut cells = [const { LeaseCell::EMPTY }; 4];
        let coordinator =
            ResponseCoordinator::new(spool, &plan, &mut states, &mut cells, clock.clone()).unwrap();
        test(&coordinator, &files, &clock);
    }
    fn resolve_value(
        response: &mut ResponseRetention<'_, '_, '_, '_>,
        source: std::ops::Range<u64>,
        path: &str,
    ) -> Result<std::ops::Range<u64>, crate::jmap_request::resolution::Error> {
        let limit = (source.end - source.start) as usize * 4 + path.len() + 100;
        let mut resolution =
            crate::jmap_request::resolution::Resolution::new(response, source, path, u64::MAX)?;
        let mut scratch = [0; 127];
        for _ in 0..limit {
            if resolution.poll(&mut scratch)? {
                return resolution.finish();
            }
        }
        panic!("resolution failed to make bounded progress");
    }
    #[test]
    fn retained_pointer_expansion_publishes_exact_values_in_the_same_file() {
        run(|coordinator, _, _| {
            let mut response = coordinator.retain_response(deadline()).unwrap();
            response.append(b"{}").unwrap();
            let source = br#"{"list":[{"ids":["m2","m1"]},{"ids":["m3"]}]}"#;
            response.append(source).unwrap();
            response.publish().unwrap();
            let range =
                resolve_value(&mut response, 2..source.len() as u64 + 2, "/list/*/ids").unwrap();
            let mut out = vec![0; (range.end - range.start) as usize];
            response.read_at(range.start, &mut out).unwrap();
            assert_eq!(out, br#"["m2","m1","m3"]"#);
            assert_eq!(response.published_len(), range.end);
            let second = resolve_value(&mut response, range, "/1").unwrap();
            let mut out = vec![0; (second.end - second.start) as usize];
            response.read_at(second.start, &mut out).unwrap();
            assert_eq!(out, br#""m1""#);
            let mut unchanged = vec![0; source.len()];
            response.read_at(2, &mut unchanged).unwrap();
            assert_eq!(unchanged, source);
        });
    }
    #[test]
    fn invalid_late_reference_and_capacity_failure_discard_only_their_tail() {
        use crate::jmap_request::{pointer, resolution};
        run(|coordinator, _, _| {
            let mut response = coordinator.retain_response(deadline()).unwrap();
            let source = br#"[{"x":1},{}]"#;
            response.append(source).unwrap();
            response.publish().unwrap();
            assert_eq!(
                resolve_value(&mut response, 0..source.len() as u64, "/*/x"),
                Err(resolution::Error::Pointer(
                    pointer::Error::InvalidResultReference
                ))
            );
            assert_eq!(response.len(), source.len() as u64);
            assert_eq!(response.failure(), None);
            resolve_value(&mut response, 0..source.len() as u64, "").unwrap();
        });
        run(|coordinator, _, _| {
            let mut response = coordinator.retain_response(deadline()).unwrap();
            let source = format!("\"{}\"", "x".repeat(40000));
            response.append(source.as_bytes()).unwrap();
            response.publish().unwrap();
            assert_eq!(
                resolve_value(&mut response, 0..source.len() as u64, ""),
                Err(resolution::Error::Store(ports::Error::Capacity))
            );
            assert_eq!(response.len(), source.len() as u64);
            assert_eq!(response.published_len(), source.len() as u64);
            assert_eq!(response.failure(), None);
            assert_eq!(response.work_failure(), None);
            let mut original = vec![0; source.len()];
            response.read_at(0, &mut original).unwrap();
            assert_eq!(original, source.as_bytes());
        });
    }
    #[test]
    fn resolution_cancel_drop_and_incomplete_finish_preserve_published_source() {
        use crate::jmap_request::resolution::{Error, Resolution};
        for how in 0..3 {
            run(|coordinator, _, _| {
                let mut response = coordinator.retain_response(deadline()).unwrap();
                let source = format!("[{}]", vec![r#"{"x":1}"#; 100].join(","));
                response.append(source.as_bytes()).unwrap();
                response.publish().unwrap();
                let mut resolution =
                    Resolution::new(&mut response, 0..source.len() as u64, "/*/x", u64::MAX)
                        .unwrap();
                for _ in 0..20 {
                    assert!(!resolution.poll(&mut [0; 16]).unwrap());
                }
                match how {
                    0 => resolution.cancel().unwrap(),
                    1 => drop(resolution),
                    _ => assert_eq!(resolution.finish(), Err(Error::State)),
                }
                assert_eq!(response.len(), source.len() as u64);
                assert_eq!(response.published_len(), source.len() as u64);
                assert_eq!(response.failure(), None);
            });
        }
    }
    #[test]
    fn cpu_only_resolution_turns_keep_original_deadline() {
        use crate::jmap_request::resolution::{Error, Resolution};
        run(|coordinator, _, clock| {
            let mut response = coordinator.retain_response(deadline()).unwrap();
            response.append(b"{}").unwrap();
            response.publish().unwrap();
            let path = format!("/{}", "x".repeat(2000));
            let mut resolution = Resolution::new(&mut response, 0..2, &path, u64::MAX).unwrap();
            assert!(!resolution.poll(&mut [0; 16]).unwrap());
            clock.0.store(100, Ordering::SeqCst);
            assert!(matches!(
                resolution.poll(&mut [0; 16]),
                Err(Error::Store(ports::Error::Deadline))
            ));
            drop(resolution);
            assert!(response.failure().is_some());
        });
    }
    #[test]
    fn expansion_ceiling_preserves_mandatory_space() {
        run(|coordinator, _, _| {
            let mut response = coordinator.retain_response(deadline()).unwrap();
            response.append(b"[1,2,3]").unwrap();
            response.publish().unwrap();
            let mut resolution = Resolution::new(&mut response, 0..7, "/*", 4).unwrap();
            let mut refused = false;
            for _ in 0..64 {
                match resolution.poll(&mut [0; 16]) {
                    Err(Error::Store(ports::Error::Capacity)) => {
                        refused = true;
                        break;
                    }
                    Ok(false) => (),
                    other => panic!("unexpected progress: {other:?}"),
                }
            }
            assert!(refused);
            drop(resolution);
            assert_eq!(response.len(), 7);
            assert_eq!(response.published_len(), 7);
            assert_eq!(response.failure(), None);
            response.append(br#"{"type":"serverFail"}"#).unwrap();
            response.publish().unwrap();
        });
        assert!(std::mem::size_of::<Resolution<'_, '_, '_, '_, '_, '_>>() <= 24 * 1024);
    }
    #[test]
    fn invalid_scratch_and_truncated_array_never_publish_partial_output() {
        run(|coordinator, _, _| {
            let mut response = coordinator.retain_response(deadline()).unwrap();
            response.append(b"[1,2]").unwrap();
            response.publish().unwrap();
            let mut resolution = Resolution::new(&mut response, 0..5, "/*", u64::MAX).unwrap();
            assert_eq!(resolution.poll(&mut []), Err(Error::State));
            assert_eq!(resolution.poll(&mut [0; 16]), Err(Error::State));
            assert_eq!(resolution.cancel(), Err(Error::State));
            assert_eq!(response.len(), 5);
            assert_eq!(response.published_len(), 5);
        });
        run(|coordinator, _, _| {
            let mut response = coordinator.retain_response(deadline()).unwrap();
            response.append(b"[1,2").unwrap();
            response.publish().unwrap();
            assert!(matches!(
                resolve_value(&mut response, 0..4, "/*"),
                Err(Error::Pointer(pointer::Error::Source(_)))
            ));
            assert_eq!(response.len(), 4);
            assert_eq!(response.published_len(), 4);
        });
    }
    #[test]
    fn cancellation_reports_prior_cleanup_failure() {
        run(|coordinator, _, clock| {
            let mut response = coordinator.retain_response(deadline()).unwrap();
            response.append(b"[1,2]").unwrap();
            response.publish().unwrap();
            let mut resolution = Resolution::new(&mut response, 0..5, "/*", u64::MAX).unwrap();
            assert!(!resolution.poll(&mut [0; 16]).unwrap());
            clock.0.store(100, Ordering::SeqCst);
            let error = resolution.poll(&mut [0; 16]).unwrap_err();
            assert!(matches!(error, Error::Store(_)));
            assert_eq!(resolution.cancel(), Err(error));
            assert_eq!(response.published_len(), 5);
            assert!(response.failure().is_some());
        });
    }
    #[test]
    fn completion_stays_unpublished_until_fresh_finish_admission() {
        for how in 0..3 {
            run(|coordinator, _, clock| {
                let mut response = coordinator.retain_response(deadline()).unwrap();
                response.append(b"{}").unwrap();
                response.publish().unwrap();
                let mut resolution = Resolution::new(&mut response, 0..2, "", u64::MAX).unwrap();
                let mut complete = false;
                for _ in 0..32 {
                    if resolution.poll(&mut [0; 16]).unwrap() {
                        complete = true;
                        break;
                    }
                }
                assert!(complete);
                assert_eq!(resolution.retention.published_len(), 2);
                assert_eq!(resolution.retention.len(), 4);
                if how == 2 {
                    clock.0.store(100, Ordering::SeqCst);
                    assert!(matches!(resolution.finish(), Err(Error::Store(_))));
                    assert_eq!(response.published_len(), 2);
                    // Expired owners refuse truncation too; the unpublished
                    // tail remains private until whole-file cleanup.
                    assert!(response.failure().is_some());
                } else {
                    if how == 0 {
                        resolution.cancel().unwrap();
                    } else {
                        drop(resolution);
                    }
                    assert_eq!(response.published_len(), 2);
                    assert_eq!(response.len(), 2);
                }
            });
        }
    }
    #[test]
    fn copy_phase_io_failure_keeps_the_tail_unpublished() {
        run(|coordinator, files, _| {
            let mut response = coordinator.retain_response(deadline()).unwrap();
            response.append(b"[1]").unwrap();
            response.publish().unwrap();
            let mut resolution = Resolution::new(&mut response, 0..3, "/*", 32).unwrap();
            for _ in 0..32 {
                if matches!(resolution.phase, Phase::Copy { .. }) {
                    break;
                }
                assert!(!resolution.poll(&mut [0; 16]).unwrap());
            }
            assert!(matches!(resolution.phase, Phase::Copy { .. }));
            let file = fs::OpenOptions::new()
                .write(true)
                .open(files.path.join("request-00/response"))
                .unwrap();
            file.set_len(0).unwrap();
            assert!(matches!(
                resolution.poll(&mut [0; 16]),
                Err(Error::Store(_))
            ));
            drop(resolution);
            assert_eq!(response.published_len(), 3);
            assert!(response.failure().is_some());
        });
    }
    #[test]
    fn resolution_binds_published_extent_and_io_failures_stay_storage_errors() {
        use crate::jmap_request::resolution::{Error, Resolution};
        run(|coordinator, files, _| {
            let mut response = coordinator.retain_response(deadline()).unwrap();
            response.append(b"{}").unwrap();
            assert!(matches!(
                Resolution::new(&mut response, 0..2, "", u64::MAX),
                Err(Error::State)
            ));
            response.publish().unwrap();
            assert!(matches!(
                Resolution::new(&mut response, 0..3, "", u64::MAX),
                Err(Error::State)
            ));
            assert!(matches!(
                Resolution::new(&mut response, 1..1, "", u64::MAX),
                Err(Error::State)
            ));
            response.append(b"pending").unwrap();
            assert!(matches!(
                Resolution::new(&mut response, 0..2, "", u64::MAX),
                Err(Error::State)
            ));
            response.rollback_tail().unwrap();
            let file = fs::OpenOptions::new()
                .write(true)
                .open(files.path.join("request-00/response"))
                .unwrap();
            file.set_len(0).unwrap();
            assert!(matches!(
                resolve_value(&mut response, 0..2, ""),
                Err(Error::Store(_))
            ));
            assert!(response.failure().is_some());
        });
    }
}
