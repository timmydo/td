//! Native scratch and cumulative work custody for the incremental sorter.
use super::*;
use crate::{
    admission::work::{Charge as WorkCharge, Meter},
    external_sort::{self, File, Next, Output, MAX_MERGE_RUNS, MAX_RECORD_BYTES, MAX_RUN_RECORDS},
    store_fs::RequestSpool,
};
const TURN_RECORDS: u64 = 256;
const TURN_BYTES: u64 = crate::limits::SQLITE_BODY_CHUNK_BYTES as u64;
const _: () = assert!(
    (MAX_RUN_RECORDS + MAX_MERGE_RUNS) * MAX_RECORD_BYTES + 2 * MAX_RECORD_BYTES + 4096 <= 1 << 20
);

use std::{
    cmp::Ordering,
    sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
};

/// One callback's additional source/comparator work uses the original job meter.
/// Call before each bounded operation, including source lookups and decoding.
pub trait SortWork {
    fn charge(&mut self, io_bytes: u64, records: u64) -> Result<(), ports::Error>;
}
/// Trusted source on one retained read view. A step must yield within 64 KiB
/// and 256 records; both cumulative charge ceilings are enforced. Never renew
/// the view, deadline or budget while a job is active.
pub trait SortSource<const N: usize> {
    fn next(&mut self, record: &mut [u8; N], work: &mut dyn SortWork)
        -> Result<Next, ports::Error>;
    fn compare(
        &mut self,
        a: &[u8; N],
        b: &[u8; N],
        work: &mut dyn SortWork,
    ) -> Result<Ordering, ports::Error>;
}
/// The coordinator admits only one resident sort workspace at a time.
/// Its borrowed buffers cannot outlive the permit or be enlarged by a caller.
pub struct SortWorkspace<'c, const N: usize> {
    permit: &'c AtomicBool,
    run: Vec<[u8; N]>,
    heads: Vec<[u8; N]>,
}
impl<const N: usize> SortWorkspace<'_, N> {
    pub fn buffers(&mut self) -> (&mut [[u8; N]], &mut [[u8; N]]) {
        (&mut self.run, &mut self.heads)
    }
}
impl<const N: usize> Drop for SortWorkspace<'_, N> {
    fn drop(&mut self) {
        // Free the charged resident bytes before allowing another allocation.
        self.run = Vec::new();
        self.heads = Vec::new();
        self.permit.store(false, AtomicOrdering::Release);
    }
}
struct Budget<'c, 'r, 'a, P> {
    coordinator: &'c StoreCoordinator<'r, 'a, P>,
    deadline: Deadline,
    last: Tick,
    meter: Meter,
    failed: Option<ports::Error>,
}
impl<P> Budget<'_, '_, '_, P> {
    fn record<T>(&mut self, result: Result<T, ports::Error>) -> Result<T, ports::Error> {
        result.inspect_err(|error| {
            self.failed.get_or_insert(*error);
        })
    }
    fn check(&mut self) -> Result<(), ports::Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.coordinator.try_state().and_then(|state| {
            if state.stopped {
                Err(ports::Error::WriterStopped)
            } else {
                Ok(())
            }
        });
        self.record(result)?;
        let result = check_time(&self.coordinator.store, self.deadline, &mut self.last).map(|_| ());
        self.record(result)
    }
}
impl<P> SortWork for Budget<'_, '_, '_, P> {
    fn charge(&mut self, io_bytes: u64, records: u64) -> Result<(), ports::Error> {
        self.check()?;
        if records == 0 && io_bytes == 0 {
            return self.record(Err(ports::Error::Invalid));
        }
        if records > TURN_RECORDS || io_bytes > TURN_BYTES {
            return self.record(Err(ports::Error::Capacity));
        }
        let result = self
            .meter
            .charge(
                self.last,
                WorkCharge {
                    io_bytes,
                    records,
                    output_bytes: 0,
                    unlinks: 0,
                },
            )
            .map_err(|error| match error {
                crate::admission::work::Stop::Deadline => ports::Error::Deadline,
                _ => ports::Error::Capacity,
            });
        self.record(result)?;
        self.check()
    }
}
struct Callback<'b, 'c, 'r, 'a, P> {
    budget: &'b mut Budget<'c, 'r, 'a, P>,
    bytes: u64,
    records: u64,
}
impl<'b, 'c, 'r, 'a, P> Callback<'b, 'c, 'r, 'a, P> {
    fn new(budget: &'b mut Budget<'c, 'r, 'a, P>) -> Self {
        Self {
            budget,
            bytes: 0,
            records: 0,
        }
    }
}
impl<P> SortWork for Callback<'_, '_, '_, '_, P> {
    fn charge(&mut self, bytes: u64, records: u64) -> Result<(), ports::Error> {
        self.budget.check()?;
        self.bytes = self.bytes.saturating_add(bytes);
        self.records = self.records.saturating_add(records);
        if self.bytes > TURN_BYTES || self.records > TURN_RECORDS {
            return self.budget.record(Err(ports::Error::Capacity));
        }
        self.budget.charge(bytes, records)
    }
}
/// Owns two charged files and one original work/deadline budget across passes.
/// The source owns authorization and the pinned view; completion still requires
/// its fresh publication validation. Dropping this owner cleans both files.
pub struct NativeSortJob<'c, 's, 'r, 'sr, 'a, const N: usize, P, S> {
    files: [SortRetention<'c, 's, 'r, 'sr, 'a, P>; 2],
    budget: Budget<'c, 'r, 'a, P>,
    source: S,
}
impl<'r, 'a, P> StoreCoordinator<'r, 'a, P> {
    pub fn sort_job<'c, 's, 'sr, const N: usize, S: SortSource<N>>(
        &'c self,
        spool: &'s RequestSpool<'sr>,
        run_records: usize,
        file_bytes: u64,
        deadline: Deadline,
        source: S,
    ) -> Result<
        (
            SortWorkspace<'c, N>,
            NativeSortJob<'c, 's, 'r, 'sr, 'a, N, P, S>,
        ),
        SortError,
    > {
        if N == 0
            || N > MAX_RECORD_BYTES
            || run_records == 0
            || run_records > MAX_RUN_RECORDS
            || file_bytes == 0
        {
            return Err(ports::Error::Invalid.into());
        }
        if file_bytes > self.sort_bytes / 2 {
            return Err(ports::Error::Capacity.into());
        }
        let mut last = self.store.clock.sample()?.monotonic;
        check_time(&self.store, deadline, &mut last)?;
        if self.try_state()?.stopped {
            return Err(ports::Error::WriterStopped.into());
        }
        self.sort_workspace
            .compare_exchange(false, true, AtomicOrdering::AcqRel, AtomicOrdering::Acquire)
            .map_err(|_| ports::Error::Busy)?;
        let mut workspace = SortWorkspace {
            permit: &self.sort_workspace,
            run: Vec::new(),
            heads: Vec::new(),
        };
        workspace
            .run
            .try_reserve_exact(run_records)
            .map_err(|_| ports::Error::Capacity)?;
        workspace
            .heads
            .try_reserve_exact(MAX_MERGE_RUNS)
            .map_err(|_| ports::Error::Capacity)?;
        workspace.run.resize(run_records, [0; N]);
        workspace.heads.resize(MAX_MERGE_RUNS, [0; N]);
        let a = self.retain_sort(spool, file_bytes, deadline)?;
        let b = self.retain_sort(spool, file_bytes, deadline)?;
        let mut budget = Budget {
            coordinator: self,
            deadline,
            last,
            meter: Meter::new(
                deadline,
                WorkCharge {
                    io_bytes: self.work.foreground_io_bytes,
                    records: self.work.foreground_records,
                    output_bytes: 0,
                    unlinks: 0,
                },
            ),
            failed: None,
        };
        budget.charge(0, 1)?;
        Ok((
            workspace,
            NativeSortJob {
                files: [a, b],
                budget,
                source,
            },
        ))
    }
}
impl<'c, 's, 'r, 'sr, 'a, const N: usize, P, S> NativeSortJob<'c, 's, 'r, 'sr, 'a, N, P, S> {
    fn file(
        &mut self,
        file: File,
    ) -> Result<&mut SortRetention<'c, 's, 'r, 'sr, 'a, P>, ports::Error> {
        self.files
            .get_mut(match file {
                File::A => 0,
                File::B => 1,
            })
            .ok_or(ports::Error::Invalid)
    }
    #[cfg(test)]
    pub(crate) fn test_record_budget(&mut self, records: u64) {
        self.budget.meter = Meter::new(
            self.budget.deadline,
            WorkCharge {
                records,
                io_bytes: u64::MAX,
                output_bytes: 0,
                unlinks: 0,
            },
        );
    }
    /// For fresh view/authorization validation, never replacement of job inputs.
    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }
    pub fn failure(&self) -> Option<ports::Error> {
        self.budget.failed
    }
    #[cfg(test)]
    pub(crate) fn test_remaining_records(&self) -> u64 {
        self.budget.meter.remaining().records
    }
    /// Read trusted scratch coordinates. This checks bounds and sealed length,
    /// not provenance or algorithm completion: callers must supply the Output
    /// returned by their completed sorter on this very job, never intermediate
    /// run coordinates or an output from another job.
    pub fn read_output(
        &mut self,
        output: Output,
        record: u64,
        bytes: &mut [u8; N],
    ) -> Result<(), ports::Error> {
        self.budget.charge(N as u64, 1)?;
        let result = (|| {
            if record >= output.records {
                return Err(ports::Error::Invalid);
            }
            let length = output
                .records
                .checked_mul(N as u64)
                .ok_or(ports::Error::Capacity)?;
            let offset = record.checked_mul(N as u64).ok_or(ports::Error::Capacity)?;
            let file = self.file(output.file)?;
            if file.sealed_len() != length {
                return Err(ports::Error::Invalid);
            }
            file.read_at(offset, bytes)
        })();
        self.budget.record(result)?;
        self.budget.check()
    }
}
impl<const N: usize, P, S: SortSource<N>> external_sort::Job<N>
    for NativeSortJob<'_, '_, '_, '_, '_, N, P, S>
{
    type Error = ports::Error;
    fn admit(&mut self, io_bytes: u64) -> Result<(), ports::Error> {
        self.budget.charge(io_bytes, 1)
    }
    fn next(&mut self, record: &mut [u8; N]) -> Result<Next, ports::Error> {
        self.budget.charge(0, 1)?;
        let result = self
            .source
            .next(record, &mut Callback::new(&mut self.budget));
        let value = self.budget.record(result)?;
        self.budget.check()?;
        Ok(value)
    }
    fn compare(&mut self, a: &[u8; N], b: &[u8; N]) -> Result<Ordering, ports::Error> {
        self.budget.charge(0, 1)?;
        let result = self
            .source
            .compare(a, b, &mut Callback::new(&mut self.budget));
        let value = self.budget.record(result)?;
        self.budget.check()?;
        Ok(value)
    }
    fn append(&mut self, file: File, record: &[u8; N]) -> Result<(), ports::Error> {
        self.budget.charge(N as u64, 1)?;
        let result = self.file(file).and_then(|file| file.append(record));
        self.budget.record(result)?;
        self.budget.check()
    }
    fn read(&mut self, file: File, offset: u64, record: &mut [u8; N]) -> Result<(), ports::Error> {
        self.budget.charge(N as u64, 1)?;
        let result = self
            .file(file)
            .and_then(|file| file.read_at(offset, record));
        self.budget.record(result)?;
        self.budget.check()
    }
    fn reset(&mut self, file: File) -> Result<(), ports::Error> {
        self.budget.charge(0, 1)?;
        let result = self.file(file).and_then(|file| file.reset());
        self.budget.record(result)?;
        self.budget.check()
    }
    fn seal(&mut self, file: File) -> Result<u64, ports::Error> {
        self.budget.charge(0, 1)?;
        let result = self.file(file).and_then(|file| file.seal());
        let length = self.budget.record(result)?;
        self.budget.check()?;
        Ok(length)
    }
}
