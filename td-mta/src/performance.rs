//! Fixed, process-local diagnostic counters. No observation grants authority.
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum Phase {
    DbResolve,
    DbConnect,
    DbTcp,
    DbHandshake,
    DbTls,
    DbSchema,
    DbRuntime,
    DbWork,
    DbReuse,
    HttpAuth,
    HttpApi,
    HttpDownload,
    HttpUpload,
    HttpSession,
    HttpOther,
    BodyVerify,
    BodyReplay,
    SmtpAdmission,
    SmtpCommit,
    DbClose,
    HttpTls,
    HttpBodyBusy,
    HttpAuthRefused,
    SmtpData,
    SmtpCommitWait,
}
impl Phase {
    pub const ALL: &'static [Self] = &[
        Self::DbResolve,
        Self::DbConnect,
        Self::DbTcp,
        Self::DbHandshake,
        Self::DbTls,
        Self::DbSchema,
        Self::DbRuntime,
        Self::DbWork,
        Self::DbReuse,
        Self::HttpAuth,
        Self::HttpApi,
        Self::HttpDownload,
        Self::HttpUpload,
        Self::HttpSession,
        Self::HttpOther,
        Self::BodyVerify,
        Self::BodyReplay,
        Self::SmtpAdmission,
        Self::SmtpCommit,
        Self::DbClose,
        Self::HttpTls,
        Self::HttpBodyBusy,
        Self::HttpAuthRefused,
        Self::SmtpData,
        Self::SmtpCommitWait,
    ];
    pub const fn code(self) -> &'static str {
        match self {
            Self::DbResolve => "db_resolve",
            Self::DbConnect => "db_connect",
            Self::DbTcp => "db_tcp",
            Self::DbHandshake => "db_handshake",
            Self::DbTls => "db_tls",
            Self::DbSchema => "db_schema",
            Self::DbRuntime => "db_runtime",
            Self::DbWork => "db_work",
            Self::DbReuse => "db_reuse",
            Self::HttpAuth => "http_auth",
            Self::HttpApi => "http_api",
            Self::HttpDownload => "http_download",
            Self::HttpUpload => "http_upload",
            Self::HttpSession => "http_session",
            Self::HttpOther => "http_other",
            Self::BodyVerify => "body_verify",
            Self::BodyReplay => "body_replay",
            Self::SmtpAdmission => "smtp_admission",
            Self::SmtpCommit => "smtp_commit",
            Self::DbClose => "db_close",
            Self::HttpTls => "http_tls",
            Self::HttpBodyBusy => "http_body_busy",
            Self::HttpAuthRefused => "http_auth_refused",
            Self::SmtpData => "smtp_data",
            Self::SmtpCommitWait => "smtp_commit_wait",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Success,
    Failed,
    Busy,
    Unavailable,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Sample {
    pub calls: u64,
    pub failures: u64,
    pub busy: u64,
    pub unavailable: u64,
    pub total_us: u64,
    pub max_us: u64,
    pub lost: u64,
}
struct Cell {
    sample: Mutex<Sample>,
    lost: AtomicU64,
}
impl Cell {
    const fn new() -> Self {
        Self {
            sample: Mutex::new(Sample {
                calls: 0,
                failures: 0,
                busy: 0,
                unavailable: 0,
                total_us: 0,
                max_us: 0,
                lost: 0,
            }),
            lost: AtomicU64::new(0),
        }
    }
    fn record(&self, duration: Duration, outcome: Outcome) {
        let Ok(mut sample) = self.sample.try_lock() else {
            let _ = self
                .lost
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    Some(n.saturating_add(1))
                });
            return;
        };
        let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        sample.calls = sample.calls.saturating_add(1);
        sample.failures = sample
            .failures
            .saturating_add(u64::from(outcome == Outcome::Failed));
        sample.busy = sample
            .busy
            .saturating_add(u64::from(outcome == Outcome::Busy));
        sample.unavailable = sample
            .unavailable
            .saturating_add(u64::from(outcome == Outcome::Unavailable));
        sample.total_us = sample.total_us.saturating_add(micros);
        sample.max_us = sample.max_us.max(micros);
    }
    fn snapshot(&self) -> Option<Sample> {
        let mut sample = *self.sample.try_lock().ok()?;
        sample.lost = self.lost.load(Ordering::Relaxed);
        Some(sample)
    }
}
// Length is the enum's fixed index domain, not an independently maintained list.
static CELLS: [Cell; Phase::ALL.len()] = [const { Cell::new() }; Phase::ALL.len()];
pub fn record(phase: Phase, duration: Duration, outcome: Outcome) {
    if let Some(cell) = CELLS.get(phase as usize) {
        cell.record(duration, outcome);
    }
}
pub(crate) fn snapshot(phase: Phase) -> Option<Sample> {
    CELLS.get(phase as usize).and_then(Cell::snapshot)
}
/// Start at an operation boundary, never in per-byte or driver polling loops.
pub struct Timer {
    phase: Phase,
    start: Instant,
    outcome: Outcome,
}
impl Timer {
    pub fn start(phase: Phase) -> Self {
        Self {
            phase,
            start: Instant::now(),
            outcome: Outcome::Failed,
        }
    }
    pub fn succeeded(&mut self) {
        self.outcome = Outcome::Success;
    }
    pub fn finish(mut self, outcome: Outcome) {
        self.outcome = outcome;
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        record(self.phase, self.start.elapsed(), self.outcome);
    }
}

pub const MAX_STATUS_BYTES: usize = 8192;

pub(crate) fn encode_status(
    output: &mut crate::bounded::TextBuffer<'_>,
    boot: crate::ids::BootId,
    generation: u64,
    utc_ms: Option<i64>,
) -> Result<(), crate::bounded::Error> {
    let samples =
        std::array::from_fn(|index| Phase::ALL.get(index).and_then(|phase| snapshot(*phase)));
    encode_samples(output, boot, generation, utc_ms, &samples)
}

fn encode_samples(
    output: &mut crate::bounded::TextBuffer<'_>,
    boot: crate::ids::BootId,
    generation: u64,
    utc_ms: Option<i64>,
    samples: &[Option<Sample>; Phase::ALL.len()],
) -> Result<(), crate::bounded::Error> {
    struct Json<'a> {
        boot: crate::ids::BootId,
        generation: u64,
        utc_ms: Option<i64>,
        samples: &'a [Option<Sample>; Phase::ALL.len()],
    }
    impl std::fmt::Display for Json<'_> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{{\"schema\":1,\"command\":\"performance-status\",\"status\":\"ok\",\"boot_id\":\"{}\",\"config_generation\":{},\"utc_ms\":", self.boot, self.generation)?;
            match self.utc_ms {
                Some(value) => write!(f, "{value}"),
                None => f.write_str("null"),
            }?;
            f.write_str(",\"phases\":{")?;
            for (index, (phase, sample)) in Phase::ALL.iter().zip(self.samples).enumerate() {
                if index != 0 {
                    f.write_str(",")?;
                }
                write!(f, "\"{}\":", phase.code())?;
                match sample {
                    None => f.write_str("null")?,
                    Some(sample) => write!(f, "{{\"calls\":{},\"failures\":{},\"busy\":{},\"unavailable\":{},\"total_us\":{},\"max_us\":{},\"lost\":{}}}", sample.calls, sample.failures, sample.busy, sample.unavailable, sample.total_us, sample.max_us, sample.lost)?,
                }
            }
            f.write_str("}}\n")
        }
    }
    output.format(format_args!(
        "{}",
        Json {
            boot,
            generation,
            utc_ms,
            samples
        }
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    #[test]
    fn complete_status_fits_its_bound_and_marks_unavailable_samples() {
        let maximum = Sample {
            calls: u64::MAX,
            failures: u64::MAX,
            busy: u64::MAX,
            unavailable: u64::MAX,
            total_us: u64::MAX,
            max_us: u64::MAX,
            lost: u64::MAX,
        };
        let samples = [Some(maximum); Phase::ALL.len()];
        let mut bytes = [0; MAX_STATUS_BYTES];
        let mut output = crate::bounded::TextBuffer::new(&mut bytes);
        let boot = crate::ids::BootId::from_bytes([255; 16]);
        encode_samples(&mut output, boot, u64::MAX, Some(i64::MIN), &samples).unwrap();
        let json = td_json::parse(output.as_str().unwrap()).unwrap();
        assert_eq!(json["command"].as_str(), Some("performance-status"));
        for phase in Phase::ALL {
            assert_eq!(
                json["phases"][phase.code()]["max_us"].as_u64(),
                Some(u64::MAX)
            );
        }
        let needed = output.as_str().unwrap().len();
        let mut short = vec![0; needed - 1];
        let mut short = crate::bounded::TextBuffer::new(&mut short);
        assert!(encode_samples(&mut short, boot, 0, None, &samples).is_ok());
        short.clear();
        assert!(encode_samples(&mut short, boot, u64::MAX, Some(i64::MIN), &samples).is_err());
        assert_eq!(short.as_str().unwrap(), "");
        output.clear();
        encode_samples(&mut output, boot, 0, None, &[None; Phase::ALL.len()]).unwrap();
        let json = td_json::parse(output.as_str().unwrap()).unwrap();
        for phase in Phase::ALL {
            assert!(json["phases"][phase.code()].is_null());
        }
    }

    #[test]
    fn cumulative_observations_survive_snapshots_and_saturate() {
        let cell = Cell::new();
        cell.record(Duration::from_micros(50), Outcome::Success);
        cell.record(Duration::from_micros(150), Outcome::Busy);
        cell.record(Duration::from_micros(100), Outcome::Failed);
        let sample = cell.snapshot().unwrap();
        assert_eq!(
            sample,
            Sample {
                calls: 3,
                failures: 1,
                busy: 1,
                unavailable: 0,
                total_us: 300,
                max_us: 150,
                lost: 0
            }
        );
        assert_eq!(cell.snapshot(), Some(sample));
        *cell.sample.lock().unwrap() = Sample {
            calls: u64::MAX,
            total_us: u64::MAX,
            ..sample
        };
        cell.record(Duration::MAX, Outcome::Success);
        let saturated = cell.snapshot().unwrap();
        assert_eq!(saturated.calls, u64::MAX);
        assert_eq!(saturated.total_us, u64::MAX);
        assert_eq!(saturated.max_us, u64::MAX);
    }
    #[test]
    fn busy_counter_never_waits_for_a_sample_lock() {
        let cell = Cell::new();
        let guard = cell.sample.lock().unwrap();
        cell.record(Duration::ZERO, Outcome::Success);
        assert_eq!(cell.snapshot(), None);
        drop(guard);
        assert_eq!(cell.snapshot().unwrap().lost, 1);
        assert_eq!(cell.snapshot().unwrap().calls, 0);
    }
    #[test]
    fn phase_table_is_complete_unique_and_small() {
        assert!(std::mem::size_of_val(&CELLS) <= 2048);
        for (index, phase) in Phase::ALL.iter().enumerate() {
            assert_eq!(*phase as usize, index);
            assert_eq!(
                Phase::ALL
                    .iter()
                    .filter(|p| p.code() == phase.code())
                    .count(),
                1
            );
        }
    }
}
