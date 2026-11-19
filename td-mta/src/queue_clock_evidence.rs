//! Same-boot expiry evidence under the serving profile's private directory lock.
use super::{Error, Guard, RetryFloor, DISCONTINUITY_MS};
use crate::{
    bounded::TextBuffer,
    ids::StoreEpoch,
    ports::{Clock, Tick, Time},
    postgres_probe::{Endpoint, Request},
};
use std::{
    fs,
    io::Read,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
};
use td_crypto::{Crypto, Digest, Provider};

/// Inclusive range of database UTC projected from the trusted validation.
/// The relay knows awake time within a process. Between processes it knows
/// only an upper bound (boot time also includes suspend).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Range {
    pub low: i64,
    pub high: i64,
}

impl Range {
    pub(super) fn point(utc: i64) -> Self {
        Self {
            low: utc,
            high: utc,
        }
    }

    pub(super) fn agrees(self, utc: i64, elapsed: u64) -> bool {
        [self.low, self.high].into_iter().all(|bound| {
            (i128::from(utc) - i128::from(bound) - i128::from(elapsed)).abs()
                <= i128::from(DISCONTINUITY_MS)
        })
    }
}

#[derive(Clone, Copy)]
struct Record {
    // A negative uptime marks an open or failed generation.
    uptime: i64,
    range: Range,
    // Independently retained safe UTC for conservative cleanup.
    cutoff: i64,
}
impl Record {
    fn dirty(cutoff: Option<i64>) -> Self {
        Self {
            uptime: -1,
            range: Range::point(0),
            cutoff: cutoff.unwrap_or(-1),
        }
    }
    fn decode(fields: &[u8; 32]) -> Option<Self> {
        let [uptime, low, high, cutoff] = fields.as_chunks::<8>().0 else {
            return None;
        };
        let record = Self {
            uptime: i64::from_be_bytes(*uptime),
            range: Range {
                low: i64::from_be_bytes(*low),
                high: i64::from_be_bytes(*high),
            },
            cutoff: i64::from_be_bytes(*cutoff),
        };
        (record.uptime >= -1
            && record.range.low >= 0
            && record.range.high >= record.range.low
            && record.cutoff >= -1)
            .then_some(record)
    }

    fn resume(self, stamp: Stamp, utc: i64) -> Option<Range> {
        if self.uptime < 0 || stamp.uptime < self.uptime {
            return None;
        }
        // procfs rounds down to centiseconds. The read may have occurred
        // anywhere inside the two process ticks.
        let gap = stamp
            .uptime
            .checked_sub(self.uptime)?
            .checked_add(10)?
            .checked_add_unsigned(stamp.after.0.checked_sub(stamp.before.0)?)?;
        let range = Range {
            low: self.range.low,
            high: self.range.high.checked_add(gap)?,
        };
        (gap <= DISCONTINUITY_MS as i64 && range.agrees(utc, 0)).then_some(range)
    }
}

fn digest(parts: &[&[u8]]) -> Result<[u8; 32], Error> {
    let mut hash = Provider.sha256().map_err(|_| Error::Invalid)?;
    for part in parts {
        hash.update(&(part.len() as u64).to_be_bytes())
            .map_err(|_| Error::Invalid)?;
        hash.update(part).map_err(|_| Error::Invalid)?;
    }
    hash.finish().map_err(|_| Error::Invalid)
}

fn boot_domain() -> Option<[u8; 32]> {
    let mut boot = [0; 37];
    let mut file = fs::File::open("/proc/sys/kernel/random/boot_id").ok()?;
    file.read_exact(&mut boot).ok()?;
    let [uuid @ .., b'\n'] = &boot else {
        return None;
    };
    if !uuid.iter().enumerate().all(|(i, b)| match i {
        8 | 13 | 18 | 23 => *b == b'-',
        _ => b.is_ascii_digit() || (b'a'..=b'f').contains(b),
    }) {
        return None;
    }
    let (dev, ino) = match fs::metadata("/proc/self/ns/time") {
        Ok(namespace) => (namespace.dev(), namespace.ino()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (0, 0),
        Err(_) => return None,
    };
    digest(&[uuid, &dev.to_be_bytes(), &ino.to_be_bytes()]).ok()
}

fn identity(request: &Request<'_>, epoch: StoreEpoch, profile: &str) -> Result<[u8; 32], Error> {
    let mut address = [0; 100];
    let mut text = TextBuffer::new(&mut address);
    let endpoint = match &request.address {
        Endpoint::Tcp(addr) => {
            text.format(format_args!("{addr}"))
                .map_err(|_| Error::Invalid)?;
            text.as_bytes().map_err(|_| Error::Invalid)?
        }
        Endpoint::Unix(path) => path.as_path().as_os_str().as_bytes(),
    };
    digest(&[
        b"td-mta queue clock v1",
        epoch.as_bytes(),
        profile.as_bytes(),
        endpoint,
        request.server_name.as_bytes(),
        request.database.as_bytes(),
    ])
}

#[derive(Clone, Copy)]
struct Stamp {
    uptime: i64,
    before: Tick,
    after: Tick,
}

impl Stamp {
    fn read(clock: &dyn Clock) -> Option<Self> {
        let before = clock.monotonic().ok()?;
        let mut bytes = [0; 64];
        let n = fs::File::open("/proc/uptime").ok()?.read(&mut bytes).ok()?;
        let after = clock.monotonic().ok()?;
        if after < before {
            return None;
        }
        let field = std::str::from_utf8(bytes.get(..n)?)
            .ok()?
            .split_once(' ')?
            .0;
        let (secs, centis) = field.split_once('.')?;
        if secs.is_empty()
            || centis.len() != 2
            || !secs
                .bytes()
                .chain(centis.bytes())
                .all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let uptime = secs
            .parse::<i64>()
            .ok()?
            .checked_mul(1000)?
            .checked_add(centis.parse::<i64>().ok()?.checked_mul(10)?)?;
        Some(Self {
            uptime,
            before,
            after,
        })
    }
}

pub(crate) struct Journal<'a> {
    path: &'a Path,
    scope: [u8; 32],
    domain: Option<[u8; 32]>,
    previous: Record,
}

impl<'a> Journal<'a> {
    pub(crate) fn begin(
        path: &'a Path,
        request: &Request<'_>,
        epoch: StoreEpoch,
        profile: &str,
    ) -> Result<Self, Error> {
        Self::open(path, identity(request, epoch, profile)?, boot_domain())
    }

    fn open(path: &'a Path, scope: [u8; 32], domain: Option<[u8; 32]>) -> Result<Self, Error> {
        let previous = Self::read(path, &scope, domain).unwrap_or(Record::dirty(None));
        let journal = Self {
            path,
            scope,
            domain,
            previous,
        };
        // Before any owner acquire/renew can erase the evidence of a clock
        // step by failing the worker. Only orderly release can replace this.
        journal.write(Record::dirty(
            (previous.cutoff >= 0).then_some(previous.cutoff),
        ))?;
        Ok(journal)
    }

    fn read(path: &Path, scope: &[u8; 32], domain: Option<[u8; 32]>) -> Option<Record> {
        let (mut file, meta) = td_fs::open_real_file(path, "queue clock evidence").ok()?;
        if meta.mode() & 0o7777 != 0o600
            || meta.nlink() != 1
            || meta.len() != 96
            || meta.uid() != fs::metadata(path.parent()?).ok()?.uid()
        {
            return None;
        }
        let mut stored_domain = [0; 32];
        let mut fields = [0; 32];
        let mut checksum = [0; 32];
        file.read_exact(&mut stored_domain).ok()?;
        file.read_exact(&mut fields).ok()?;
        file.read_exact(&mut checksum).ok()?;
        if digest(&[scope, &stored_domain, &fields]).ok()? != checksum {
            return None;
        }
        let mut record = Record::decode(&fields)?;
        if domain != Some(stored_domain) {
            // The prior UTC cutoff still bounds cleanup on this database.
            record.uptime = -1;
        }
        Some(record)
    }

    fn write(&self, record: Record) -> Result<(), Error> {
        let mut bytes = [0; 96];
        let (domain, payload) = bytes.split_at_mut(32);
        let (fields, checksum) = payload.split_at_mut(32);
        domain.copy_from_slice(&self.domain.unwrap_or_default());
        for (chunk, value) in fields.as_chunks_mut::<8>().0.iter_mut().zip([
            record.uptime,
            record.range.low,
            record.range.high,
            record.cutoff,
        ]) {
            chunk.copy_from_slice(&value.to_be_bytes());
        }
        checksum.copy_from_slice(&digest(&[&self.scope, domain, fields])?);
        td_fs::replace(self.path, &bytes, 0o600).map_err(|_| Error::Invalid)
    }

    pub(crate) fn confirmed(&self, utc: i64) -> Result<(), Error> {
        self.write(Record::dirty(Some(utc)))
    }

    pub(crate) fn start(
        &self,
        utc_ms: i64,
        clock: &dyn Clock,
        floor: RetryFloor,
    ) -> Result<Guard, Error> {
        let stamp = Stamp::read(clock);
        let monotonic = match stamp {
            Some(stamp) => stamp.after,
            None => clock.monotonic().map_err(|_| Error::Invalid)?,
        };
        self.restore(Time { utc_ms, monotonic }, stamp, floor)
    }

    fn restore(
        &self,
        sample: Time,
        stamp: Option<Stamp>,
        floor: RetryFloor,
    ) -> Result<Guard, Error> {
        let Time { utc_ms, monotonic } = sample;
        let mut guard = Guard::with_retry_floor(sample, floor)?;
        guard.range = stamp.and_then(|stamp| self.previous.resume(stamp, utc_ms));
        guard.retention_anchor = if guard.range.is_some() {
            Some(sample)
        } else {
            (self.previous.cutoff >= 0).then_some(Time {
                utc_ms: self.previous.cutoff.min(utc_ms),
                monotonic,
            })
        };
        Ok(guard)
    }

    pub(crate) fn finish(
        &self,
        guard: &std::sync::Mutex<Guard>,
        clock: &dyn Clock,
    ) -> Result<(), Error> {
        let stamp = self.domain.and_then(|_| Stamp::read(clock));
        let record = {
            let clock = guard.lock().map_err(|_| Error::Invalid)?;
            stamp
                .and_then(|stamp| Self::checkpoint(&clock, stamp))
                .or_else(|| clock.cleanup_cutoff().map(|utc| Record::dirty(Some(utc))))
        };
        if let Some(record) = record {
            self.write(record)?;
        }
        Ok(())
    }

    fn checkpoint(guard: &Guard, stamp: Stamp) -> Option<Record> {
        if guard.invalid || guard.paused || guard.last.monotonic > stamp.before {
            return None;
        }
        let range = guard.range?;
        let at = |utc: i64, tick: Tick, when: Tick| {
            utc.checked_add_unsigned(when.0.checked_sub(tick.0)?)
        };
        let low = at(range.low, guard.anchor.monotonic, stamp.before)?.min(at(
            guard.last.utc_ms,
            guard.last.monotonic,
            stamp.before,
        )?);
        let high = at(range.high, guard.anchor.monotonic, stamp.after)?.max(at(
            guard.last.utc_ms,
            guard.last.monotonic,
            stamp.after,
        )?);
        Some(Record {
            uptime: stamp.uptime,
            range: Range { low, high },
            cutoff: guard.last.utc_ms,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::{observe_request, ClockControl};
    use super::*;
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{Arc, Mutex},
    };

    fn time(utc_ms: i64, tick: u64) -> Time {
        Time {
            utc_ms,
            monotonic: Tick(tick),
        }
    }
    fn stamp(uptime: i64, tick: u64) -> Stamp {
        Stamp {
            uptime,
            before: Tick(tick),
            after: Tick(tick),
        }
    }
    const UTC: i64 = 1_000_000;
    const SCOPE: [u8; 32] = [1; 32];
    const BOOT: Option<[u8; 32]> = Some([3; 32]);

    #[test]
    fn only_a_clean_generation_resumes_expiry_within_the_same_identity() {
        let root = td_fs::TempDir::new("queue-clock").unwrap();
        let path = root.path().join("clock");
        let floor = RetryFloor::from_seconds(1).unwrap();
        let journal = Journal::open(&path, SCOPE, BOOT).unwrap();
        let mut guard = journal
            .restore(time(UTC, 0), Some(stamp(5000, 0)), floor)
            .unwrap();
        let control = Arc::new(ClockControl::default());
        guard.control = Some(control.clone());
        let status = guard.observe(time(UTC + 1000, 1000)).unwrap();
        assert!(status.fresh_dispatch && status.retry && !status.expiry);
        assert!(guard.retention_time().is_none());
        let guard = Mutex::new(guard);
        assert!(control.request());
        control.start_sample();
        observe_request(&guard, time(UTC + 1000, 1000), |utc| journal.confirmed(utc)).unwrap();
        let guard = guard.into_inner().unwrap();
        // A crash after this acknowledgement can still safely age cleanup.
        let dirty = Journal::read(&path, &SCOPE, BOOT).unwrap();
        assert_eq!(dirty.uptime, -1);
        assert_eq!(dirty.cutoff, UTC + 1000);
        let clean = Journal::checkpoint(&guard, stamp(6000, 1000)).unwrap();
        journal.write(clean).unwrap();

        let restart = Journal::open(&path, SCOPE, BOOT).unwrap();
        let mut guard = restart
            .restore(time(UTC + 2000, 5), Some(stamp(7000, 5)), floor)
            .unwrap();
        assert!(!guard.status().expiry);
        assert!(guard.observe(time(UTC + 3000, 1005)).unwrap().expiry);
        // Without another clean publication (including an acquire or renew
        // failure before guard construction), another process sees only dirty.
        let after_failure = Journal::open(&path, SCOPE, BOOT).unwrap();
        let mut held = after_failure
            .restore(time(UTC + 70_000_000, 0), Some(stamp(8000, 0)), floor)
            .unwrap();
        let status = held.observe(time(UTC + 70_001_000, 1000)).unwrap();
        assert!(status.fresh_dispatch && status.retry && !status.expiry);
        assert_eq!(held.retention_time().unwrap().cutoff, UTC + 2000);
        assert!(Journal::checkpoint(&held, stamp(9000, 1000)).is_none());
        // A hold observed by *any* guard caller prevents clean publication.
        guard.observe(time(UTC + 40_000_000, 1006)).unwrap();
        assert!(Journal::checkpoint(&guard, stamp(9000, 1006)).is_none());

        restart.write(clean).unwrap();
        assert!(Journal::read(&path, &[2; 32], BOOT).is_none());
        let rebooted = Journal::read(&path, &SCOPE, Some([9; 32])).unwrap();
        assert_eq!(rebooted.uptime, -1);
        assert_eq!(rebooted.cutoff, clean.cutoff);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Journal::read(&path, &SCOPE, BOOT).is_none());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        *bytes.first_mut().unwrap() ^= 1;
        fs::write(&path, bytes).unwrap();
        assert!(Journal::read(&path, &SCOPE, BOOT).is_none());
    }

    #[test]
    fn only_an_accepted_sample_can_update_the_cutoff_recovered_after_a_crash() {
        let root = td_fs::TempDir::new("queue-clock-confirmation").unwrap();
        let path = root.path().join("clock");
        let floor = RetryFloor::from_seconds(1).unwrap();
        let bootstrap = Journal::open(&path, SCOPE, BOOT).unwrap();
        bootstrap.confirmed(UTC).unwrap();
        let journal = Journal::open(&path, SCOPE, BOOT).unwrap();
        let mut clock = journal.restore(time(UTC, 0), None, floor).unwrap();
        let control = Arc::new(ClockControl::default());
        clock.control = Some(control.clone());
        clock.observe(time(UTC + 1000, 1000)).unwrap();
        assert!(control.request());
        control.start_sample();
        let stepped = UTC + 600_000;
        clock.observe(time(stepped, 1001)).unwrap();
        clock.observe(time(stepped + 1000, 2001)).unwrap();
        assert!(control.request()); // Replaces the request canceled before observation.
        let guard = Mutex::new(clock);
        let mut wrote = false;
        observe_request(&guard, time(stepped + 1001, 2002), |_| {
            wrote = true;
            Ok(())
        })
        .unwrap();
        assert!(!wrote);
        assert_eq!(Journal::read(&path, &SCOPE, BOOT).unwrap().cutoff, UTC);

        control.start_sample();
        let accepted = time(stepped + 1002, 2003);
        let jumped = accepted.utc_ms + 600_000;
        let (status, retention) = observe_request(&guard, accepted, |utc| {
            let mut live = guard.try_lock().unwrap();
            assert_eq!(live.anchor, accepted);
            assert_eq!(control.status(), "requested");
            // A crash before replacement would still see the earlier safe cutoff.
            assert_eq!(Journal::read(&path, &SCOPE, BOOT).unwrap().cutoff, UTC);
            live.observe(time(jumped, 2004))?;
            assert_eq!(control.status(), "held");
            live.observe(time(jumped + 1000, 3004))?;
            assert!(control.request());
            drop(live);
            journal.confirmed(utc)
        })
        .unwrap();
        assert!(!status.expiry);
        assert_eq!(retention.unwrap().cutoff, accepted.utc_ms + 1001);
        assert_eq!(control.status(), "requested");
        let restarted = Journal::open(&path, SCOPE, BOOT).unwrap();
        let mut clock = restarted
            .restore(time(jumped + 1000, 0), None, floor)
            .unwrap();
        assert!(!clock.observe(time(jumped + 2000, 1000)).unwrap().expiry);
        assert_eq!(
            clock.retention_time().unwrap().cutoff,
            accepted.utc_ms + 1000
        );
    }

    #[test]
    fn orderly_held_generations_carry_only_safe_cleanup_progress() {
        let root = td_fs::TempDir::new("queue-clock-cleanup").unwrap();
        let path = root.path().join("clock");
        let floor = RetryFloor::from_seconds(1).unwrap();
        let clock = crate::clock::RuntimeClock::new();
        let first = Journal::open(&path, SCOPE, BOOT).unwrap();
        first.confirmed(UTC).unwrap();
        // The wall clock has jumped far ahead. Each generation can advance
        // only by its own awake time, even if it stops before a stable interval.
        let mut db_now = UTC + 90_000_000;
        let mut cutoff = UTC;
        for elapsed in [500, 2000, 1000] {
            let journal = Journal::open(&path, SCOPE, BOOT).unwrap();
            let mut guard = journal
                .restore(time(db_now, 0), Some(stamp(5000, 0)), floor)
                .unwrap();
            db_now += elapsed;
            assert!(!guard.observe(time(db_now, elapsed as u64)).unwrap().expiry);
            journal.finish(&Mutex::new(guard), &clock).unwrap();
            let record = Journal::read(&path, &SCOPE, BOOT).unwrap();
            cutoff += elapsed;
            assert_eq!(record.uptime, -1);
            assert_eq!(record.cutoff, cutoff);
        }
    }

    #[test]
    fn agreement_bounds_both_jump_directions_suspend_and_accumulated_restart_gaps() {
        let record = Record {
            uptime: 50,
            range: Range::point(UTC),
            cutoff: UTC,
        };
        assert!(record.resume(stamp(49, 0), UTC).is_none());
        assert!(record.resume(stamp(300_051, 0), UTC + 300_001).is_none());
        for delta in [-300_001, 300_001] {
            assert!(record.resume(stamp(50, 0), UTC + delta).is_none());
        }
        // The erased awake time in two small outages accumulates. Trusting
        // the new wall time as a fresh anchor on each restart would admit.
        let first = record.resume(stamp(180_050, 0), UTC + 180_000).unwrap();
        let next = Record {
            uptime: 180_050,
            range: first,
            cutoff: UTC + 180_000,
        };
        assert!(next.resume(stamp(360_050, 0), UTC + 360_000).is_none());
        assert!(record.resume(stamp(55, 0), UTC + 300_000).is_some());
    }

    #[test]
    fn a_failed_dirty_write_cannot_acknowledge_confirmation() {
        let mut guard = Guard::new(time(UTC, 0)).unwrap();
        let control = Arc::new(ClockControl::default());
        guard.control = Some(control.clone());
        assert!(guard.observe(time(UTC + 300_000, 300_000)).unwrap().retry);
        assert!(control.request());
        control.start_sample();
        let guard = Mutex::new(guard);
        assert_eq!(
            observe_request(&guard, time(UTC + 300_000, 300_000), |_| Err(
                Error::Invalid
            )),
            Err(Error::Invalid)
        );
        assert_eq!(control.status(), "refused");
        let guard = guard.into_inner().unwrap();
        assert!(Journal::checkpoint(&guard, stamp(400_000, 300_000)).is_none());
    }
}
