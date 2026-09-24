//! Bounded diagnostic snapshots. Values are observations, never admission grants.
use super::{
    prefix,
    queue::{Counter, LossSnapshot},
    Context,
};
use crate::bounded::{self, TextBuffer};
use std::fmt;

pub const MAX_DISKS: usize = crate::admission::filesystems::MAX_FILESYSTEMS;
pub const MAX_STATUS_BYTES: usize = 4096;
pub const MAX_UNAVAILABLE_BYTES: usize = 2048;
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Local {
    pub configuration: bool,
    pub storage: bool,
    pub listeners: bool,
    pub writer_admission_open: bool,
    pub recovering: bool,
}
impl Local {
    pub const fn ready(self) -> bool {
        self.configuration
            && self.storage
            && self.listeners
            && self.writer_admission_open
            && !self.recovering
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Condition {
    #[default]
    Unknown,
    Healthy,
    Degraded,
    Disabled,
}
impl Condition {
    fn code(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Disabled => "disabled",
        }
    }
    fn impaired(self) -> bool {
        matches!(self, Self::Unknown | Self::Degraded)
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Dependencies {
    pub relay: Condition,
    pub certificate_renewal: Condition,
    pub logging: Condition,
    pub index: Condition,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    Serving,
    Degraded,
    Recovering,
    RefusingMutations,
}
impl State {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Serving => "serving",
            Self::Degraded => "degraded",
            Self::Recovering => "recovering",
            Self::RefusingMutations => "refusing_mutations",
        }
    }
}
/// Absent observations stay null. Counters are cumulative within the boot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Metrics {
    pub accepted_mail: Option<Counter>,
    pub refused_mail: Option<Counter>,
    pub tls_sessions: Option<Counter>,
    pub plain_sessions: Option<Counter>,
    pub limit_refusals: Option<Counter>,
    pub authentication_failures: Option<Counter>,
    pub active_slots: Option<u64>,
    pub queue_depth: Option<u64>,
    pub oldest_queue_age_ms: Option<u64>,
    pub unknown_outcomes: Option<u64>,
    pub index_lag_transactions: Option<u64>,
    pub certificate_expires_utc_ms: Option<i64>,
    pub logs: Option<LossSnapshot>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Inodes {
    Unknown,
    Unsupported,
    Available(u64),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Disk {
    /// Config-generation-local index, 0..=15; never a pathname or filesystem proof.
    pub index: u8,
    pub headroom_bytes: Option<u64>,
    pub inodes: Inodes,
    pub age_ms: Option<u64>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidDisks,
    InvalidMetrics,
    Encoding(bounded::Error),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDisks => f.write_str("invalid status disk snapshot"),
            Self::InvalidMetrics => f.write_str("inconsistent status metrics"),
            Self::Encoding(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidDisks | Self::InvalidMetrics => None,
            Self::Encoding(e) => Some(e),
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct HealthSnapshot<'a> {
    pub context: Context,
    pub local: Local,
    pub dependencies: Dependencies,
    pub metrics: Metrics,
    pub disks: &'a [Disk],
}
impl HealthSnapshot<'_> {
    pub fn state(&self) -> State {
        if self.local.recovering {
            State::Recovering
        } else if !self.local.ready() {
            State::RefusingMutations
        } else if [
            self.dependencies.relay,
            self.dependencies.certificate_renewal,
            self.dependencies.logging,
            self.dependencies.index,
        ]
        .iter()
        .any(|c| c.impaired())
        {
            State::Degraded
        } else {
            State::Serving
        }
    }
    pub fn encode(&self, output: &mut TextBuffer<'_>) -> Result<(), Error> {
        if self.metrics.queue_depth == Some(0)
            && self.metrics.oldest_queue_age_ms.is_some_and(|age| age != 0)
        {
            return Err(Error::InvalidMetrics);
        }
        if self.disks.len() > MAX_DISKS {
            return Err(Error::InvalidDisks);
        }
        let mut seen = 0u32;
        for disk in self.disks {
            if usize::from(disk.index) >= MAX_DISKS {
                return Err(Error::InvalidDisks);
            }
            let bit = 1u32
                .checked_shl(u32::from(disk.index))
                .ok_or(Error::InvalidDisks)?;
            if seen & bit != 0 {
                return Err(Error::InvalidDisks);
            }
            seen |= bit;
        }
        output
            .format(format_args!("{}", StatusJson(self)))
            .map_err(Error::Encoding)
    }
}
/// Main-thread fallback when no fresh worker-produced snapshot is available.
/// Its fixed empty metrics/disk shape fits the 2 KiB control-framing budget.
pub fn encode_unavailable(
    context: Context,
    recovering: bool,
    output: &mut TextBuffer<'_>,
) -> Result<(), Error> {
    HealthSnapshot {
        context,
        local: Local {
            recovering,
            ..Local::default()
        },
        dependencies: Dependencies::default(),
        metrics: Metrics::default(),
        disks: &[],
    }
    .encode(output)
}
struct StatusJson<'a, 'b>(&'a HealthSnapshot<'b>);
impl fmt::Display for StatusJson<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.0;
        prefix(f, "status", &s.context)?;
        write!(f, ",\"facts\":{{\"state\":\"{}\",\"ready\":{},\"local\":{{\"configuration\":{},\"storage\":{},\"listeners\":{},\"writer_admission_open\":{},\"recovering\":{}}},\"dependencies\":{{\"relay\":\"{}\",\"certificate_renewal\":\"{}\",\"logging\":\"{}\",\"index\":\"{}\"}},\"counters\":{{", s.state().code(), s.local.ready(), s.local.configuration, s.local.storage, s.local.listeners, s.local.writer_admission_open, s.local.recovering, s.dependencies.relay.code(), s.dependencies.certificate_renewal.code(), s.dependencies.logging.code(), s.dependencies.index.code())?;
        let m = s.metrics;
        for (i, (name, value)) in [
            ("accepted_mail", m.accepted_mail),
            ("refused_mail", m.refused_mail),
            ("tls_sessions", m.tls_sessions),
            ("plain_sessions", m.plain_sessions),
            ("limit_refusals", m.limit_refusals),
            ("authentication_failures", m.authentication_failures),
            ("dropped_logs", m.logs.map(|v| v.dropped)),
            ("log_write_failures", m.logs.map(|v| v.write_failures)),
        ]
        .into_iter()
        .enumerate()
        {
            if i != 0 {
                f.write_str(",")?;
            }
            write!(f, "\"{name}\":")?;
            match value {
                Some(v) => write!(
                    f,
                    "{{\"value\":{},\"saturated\":{}}}",
                    v.value(),
                    v.saturated()
                )?,
                None => f.write_str("null")?,
            }
        }
        f.write_str("},\"gauges\":{")?;
        for (i, (name, value)) in [
            ("active_slots", m.active_slots),
            ("queue_depth", m.queue_depth),
            ("oldest_queue_age_ms", m.oldest_queue_age_ms),
            ("unknown_outcomes", m.unknown_outcomes),
            ("index_lag_transactions", m.index_lag_transactions),
        ]
        .into_iter()
        .enumerate()
        {
            if i != 0 {
                f.write_str(",")?;
            }
            write!(f, "\"{name}\":")?;
            unsigned(f, value)?;
        }
        f.write_str(",\"certificate_expires_utc_ms\":")?;
        match m.certificate_expires_utc_ms {
            Some(v) => write!(f, "{v}")?,
            None => f.write_str("null")?,
        }
        f.write_str("},\"disks\":[")?;
        for (i, d) in s.disks.iter().enumerate() {
            if i != 0 {
                f.write_str(",")?;
            }
            write!(f, "{{\"index\":{},\"headroom_bytes\":", d.index)?;
            unsigned(f, d.headroom_bytes)?;
            f.write_str(",\"inodes\":{")?;
            match d.inodes {
                Inodes::Unknown => f.write_str("\"state\":\"unknown\",\"available\":null")?,
                Inodes::Unsupported => {
                    f.write_str("\"state\":\"unsupported\",\"available\":null")?
                }
                Inodes::Available(v) => write!(f, "\"state\":\"available\",\"available\":{v}")?,
            }
            f.write_str("},\"age_ms\":")?;
            unsigned(f, d.age_ms)?;
            f.write_str("}")?;
        }
        f.write_str("]},\"recommended_actions\":[")?;
        let mut separator = "";
        for (needed, action) in [
            (!s.local.configuration, "check_config"),
            (!s.local.storage, "inspect_storage"),
            (!s.local.listeners, "inspect_listeners"),
            (!s.local.writer_admission_open, "inspect_admission"),
            (s.local.recovering, "inspect_recovery"),
            (s.dependencies.relay.impaired(), "inspect_relay"),
            (
                s.dependencies.certificate_renewal.impaired(),
                "inspect_certificate",
            ),
            (s.dependencies.logging.impaired(), "inspect_logging"),
            (s.dependencies.index.impaired(), "inspect_index"),
        ] {
            if needed {
                write!(f, "{separator}\"{action}\"")?;
                separator = ",";
            }
        }
        f.write_str("],\"untrusted\":[]}\n")
    }
}
fn unsigned(f: &mut fmt::Formatter<'_>, value: Option<u64>) -> fmt::Result {
    match value {
        Some(v) => write!(f, "{v}"),
        None => f.write_str("null"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ids::BootId, observability::tests::fullest};
    fn context() -> Context {
        Context {
            boot: BootId::from_bytes([0; 16]),
            utc_ms: None,
            config_generation: 1,
            connection: None,
            request: None,
            transaction: None,
            submission: None,
        }
    }
    fn ready() -> Local {
        Local {
            configuration: true,
            storage: true,
            listeners: true,
            writer_admission_open: true,
            recovering: false,
        }
    }
    fn dependencies() -> Dependencies {
        Dependencies {
            relay: Condition::Healthy,
            certificate_renewal: Condition::Healthy,
            logging: Condition::Healthy,
            index: Condition::Healthy,
        }
    }
    fn snapshot() -> HealthSnapshot<'static> {
        HealthSnapshot {
            context: context(),
            local: ready(),
            dependencies: dependencies(),
            metrics: Metrics::default(),
            disks: &[],
        }
    }
    #[test]
    fn dependency_outages_do_not_change_local_readiness() {
        let baseline = snapshot();
        assert_eq!(baseline.state(), State::Serving);
        for condition in [
            Condition::Unknown,
            Condition::Healthy,
            Condition::Degraded,
            Condition::Disabled,
        ] {
            for member in 0..4 {
                let mut s = baseline;
                match member {
                    0 => s.dependencies.relay = condition,
                    1 => s.dependencies.certificate_renewal = condition,
                    2 => s.dependencies.logging = condition,
                    _ => s.dependencies.index = condition,
                }
                assert!(s.local.ready());
                assert_eq!(
                    s.state(),
                    if matches!(condition, Condition::Unknown | Condition::Degraded) {
                        State::Degraded
                    } else {
                        State::Serving
                    }
                );
                for failed in 0..5 {
                    let mut local = ready();
                    match failed {
                        0 => local.configuration = false,
                        1 => local.storage = false,
                        2 => local.listeners = false,
                        3 => local.writer_admission_open = false,
                        _ => local.recovering = true,
                    }
                    s.local = local;
                    assert!(!s.local.ready());
                    assert_eq!(
                        s.state(),
                        if failed == 4 {
                            State::Recovering
                        } else {
                            State::RefusingMutations
                        }
                    );
                }
            }
        }
        let s = HealthSnapshot {
            local: Local {
                recovering: true,
                ..Local::default()
            },
            ..baseline
        };
        assert_eq!(s.state(), State::Recovering);
    }
    #[test]
    fn unknown_metrics_are_null_and_status_schema_is_literal() -> Result<(), Error> {
        assert!(std::error::Error::source(&Error::InvalidDisks).is_none());
        assert!(std::error::Error::source(&Error::Encoding(bounded::Error::Capacity)).is_some());
        let s = snapshot();
        let mut bytes = [0; MAX_STATUS_BYTES];
        let mut out = TextBuffer::new(&mut bytes);
        s.encode(&mut out)?;
        assert_eq!(out.as_str().map_err(Error::Encoding)?,concat!(
            "{\"version\":1,\"record\":\"status\",\"boot_id\":\"00000000000000000000000000000000\",\"utc_ms\":null,\"config_generation\":1,",
            "\"facts\":{\"state\":\"serving\",\"ready\":true,\"local\":{\"configuration\":true,\"storage\":true,\"listeners\":true,\"writer_admission_open\":true,\"recovering\":false},",
            "\"dependencies\":{\"relay\":\"healthy\",\"certificate_renewal\":\"healthy\",\"logging\":\"healthy\",\"index\":\"healthy\"},",
            "\"counters\":{\"accepted_mail\":null,\"refused_mail\":null,\"tls_sessions\":null,\"plain_sessions\":null,\"limit_refusals\":null,\"authentication_failures\":null,\"dropped_logs\":null,\"log_write_failures\":null},",
            "\"gauges\":{\"active_slots\":null,\"queue_depth\":null,\"oldest_queue_age_ms\":null,\"unknown_outcomes\":null,\"index_lag_transactions\":null,\"certificate_expires_utc_ms\":null},",
            "\"disks\":[]},\"recommended_actions\":[],\"untrusted\":[]}\n"));
        Ok(())
    }
    #[test]
    fn worst_case_snapshot_fits_and_capacity_refusal_is_atomic() -> Result<(), Error> {
        let mut maximum = Counter::default();
        maximum.add(u64::MAX - 1);
        let mut disks = [Disk {
            index: 0,
            headroom_bytes: Some(u64::MAX),
            inodes: Inodes::Available(u64::MAX),
            age_ms: Some(u64::MAX),
        }; MAX_DISKS];
        for (index, disk) in disks.iter_mut().enumerate() {
            disk.index = u8::try_from(index).map_err(|_| Error::InvalidDisks)?;
        }
        let metrics = Metrics {
            accepted_mail: Some(maximum),
            refused_mail: Some(maximum),
            tls_sessions: Some(maximum),
            plain_sessions: Some(maximum),
            limit_refusals: Some(maximum),
            authentication_failures: Some(maximum),
            active_slots: Some(u64::MAX),
            queue_depth: Some(u64::MAX),
            oldest_queue_age_ms: Some(u64::MAX),
            unknown_outcomes: Some(u64::MAX),
            index_lag_transactions: Some(u64::MAX),
            certificate_expires_utc_ms: Some(i64::MIN),
            logs: Some(LossSnapshot {
                dropped: maximum,
                write_failures: maximum,
            }),
        };
        let s = HealthSnapshot {
            context: fullest(),
            local: Local {
                recovering: true,
                ..Local::default()
            },
            dependencies: Dependencies {
                relay: Condition::Degraded,
                certificate_renewal: Condition::Degraded,
                logging: Condition::Degraded,
                index: Condition::Degraded,
            },
            metrics,
            disks: &disks,
        };
        assert!(std::mem::size_of_val(&s) + std::mem::size_of_val(&disks) <= 2 * 1024);
        let mut bytes = [0; MAX_STATUS_BYTES];
        let mut out = TextBuffer::new(&mut bytes);
        s.encode(&mut out)?;
        let expected = out.as_str().map_err(Error::Encoding)?.to_owned();
        // Maximal dependency strings/actions dominate healthy/unknown/disabled.
        // Exercise every local-flag combination against the 4 KiB cache slot.
        for flags in 0u8..32 {
            let local = Local {
                configuration: flags & 1 != 0,
                storage: flags & 2 != 0,
                listeners: flags & 4 != 0,
                writer_admission_open: flags & 8 != 0,
                recovering: flags & 16 != 0,
            };
            let alternative = HealthSnapshot { local, ..s };
            out.clear();
            alternative.encode(&mut out)?;
        }

        assert!(expected.is_ascii());
        assert_eq!(expected.bytes().filter(|b| *b == b'\n').count(), 1);
        assert!(expected.contains("\"value\":18446744073709551614,\"saturated\":false"));
        assert!(expected.contains("\"recommended_actions\":[\"check_config\",\"inspect_storage\",\"inspect_listeners\",\"inspect_admission\",\"inspect_recovery\",\"inspect_relay\",\"inspect_certificate\",\"inspect_logging\",\"inspect_index\"]"));
        for capacity in [0, 1, expected.len() - 1] {
            let mut bytes = vec![0; capacity + 3];
            let mut out = TextBuffer::new(&mut bytes);
            out.append("old").map_err(Error::Encoding)?;
            assert_eq!(
                s.encode(&mut out),
                Err(Error::Encoding(bounded::Error::Capacity))
            );
            assert_eq!(out.as_str().map_err(Error::Encoding)?, "old");
        }
        let mut bytes = vec![0; expected.len()];
        let mut out = TextBuffer::new(&mut bytes);
        s.encode(&mut out)?;
        assert_eq!(out.as_str().map_err(Error::Encoding)?, expected);
        Ok(())
    }
    #[test]
    fn disks_distinguish_zero_unknown_and_unsupported_and_refuse_bad_identity() -> Result<(), Error>
    {
        let disks = [
            Disk {
                index: 0,
                headroom_bytes: Some(0),
                inodes: Inodes::Available(0),
                age_ms: Some(0),
            },
            Disk {
                index: 1,
                headroom_bytes: None,
                inodes: Inodes::Unknown,
                age_ms: None,
            },
            Disk {
                index: 2,
                headroom_bytes: Some(1),
                inodes: Inodes::Unsupported,
                age_ms: Some(2),
            },
        ];
        let s = HealthSnapshot {
            disks: &disks,
            ..snapshot()
        };
        let mut bytes = [0; MAX_STATUS_BYTES];
        let mut out = TextBuffer::new(&mut bytes);
        s.encode(&mut out)?;
        assert!(out.as_str().map_err(Error::Encoding)?.contains(concat!(
            "\"disks\":[{\"index\":0,\"headroom_bytes\":0,\"inodes\":{\"state\":\"available\",\"available\":0},\"age_ms\":0},",
            "{\"index\":1,\"headroom_bytes\":null,\"inodes\":{\"state\":\"unknown\",\"available\":null},\"age_ms\":null},",
            "{\"index\":2,\"headroom_bytes\":1,\"inodes\":{\"state\":\"unsupported\",\"available\":null},\"age_ms\":2}]")));
        out.clear();
        out.append("old").map_err(Error::Encoding)?;
        let disk = Disk {
            index: 0,
            headroom_bytes: None,
            inodes: Inodes::Unknown,
            age_ms: None,
        };
        for bad in [
            vec![disk; 17],
            vec![disk; 2],
            vec![Disk { index: 16, ..disk }],
        ] {
            let s = HealthSnapshot {
                disks: &bad,
                ..snapshot()
            };
            assert_eq!(s.encode(&mut out), Err(Error::InvalidDisks));
            assert_eq!(out.as_str().map_err(Error::Encoding)?, "old");
        }
        Ok(())
    }
    #[test]
    fn each_failed_fact_has_only_its_own_advisory_and_empty_queue_age_is_checked(
    ) -> Result<(), Error> {
        for failed in 0..9 {
            let mut s = snapshot();
            let action = match failed {
                0 => {
                    s.local.configuration = false;
                    "check_config"
                }
                1 => {
                    s.local.storage = false;
                    "inspect_storage"
                }
                2 => {
                    s.local.listeners = false;
                    "inspect_listeners"
                }
                3 => {
                    s.local.writer_admission_open = false;
                    "inspect_admission"
                }
                4 => {
                    s.local.recovering = true;
                    "inspect_recovery"
                }
                5 => {
                    s.dependencies.relay = Condition::Degraded;
                    "inspect_relay"
                }
                6 => {
                    s.dependencies.certificate_renewal = Condition::Degraded;
                    "inspect_certificate"
                }
                7 => {
                    s.dependencies.logging = Condition::Degraded;
                    "inspect_logging"
                }
                _ => {
                    s.dependencies.index = Condition::Degraded;
                    "inspect_index"
                }
            };
            let mut bytes = [0; MAX_STATUS_BYTES];
            let mut out = TextBuffer::new(&mut bytes);
            s.encode(&mut out)?;
            assert!(out.as_str().map_err(Error::Encoding)?.contains(&format!(
                "\"recommended_actions\":[\"{action}\"],\"untrusted\":[]"
            )));
        }
        let mut s = snapshot();
        s.metrics.queue_depth = Some(0);
        s.metrics.oldest_queue_age_ms = Some(5);
        let mut bytes = [0; MAX_STATUS_BYTES];
        let mut out = TextBuffer::new(&mut bytes);
        out.append("old").map_err(Error::Encoding)?;
        assert_eq!(s.encode(&mut out), Err(Error::InvalidMetrics));
        assert_eq!(out.as_str().map_err(Error::Encoding)?, "old");
        for age in [None, Some(0)] {
            s.metrics.oldest_queue_age_ms = age;
            out.clear();
            s.encode(&mut out)?;
        }
        Ok(())
    }
    #[test]
    fn unavailable_fallback_fits_one_main_turn_without_claiming_ready() -> Result<(), Error> {
        for recovering in [false, true] {
            let mut bytes = [0; MAX_UNAVAILABLE_BYTES];
            let mut out = TextBuffer::new(&mut bytes);
            encode_unavailable(fullest(), recovering, &mut out)?;
            let text = out.as_str().map_err(Error::Encoding)?;
            let state = if recovering {
                "recovering"
            } else {
                "refusing_mutations"
            };
            assert!(text.contains(&format!("\"state\":\"{state}\",\"ready\":false")));
            assert!(text.contains("\"accepted_mail\":null"));
            assert!(text.contains("\"disks\":[]"));
            let mut short = vec![0; text.len() - 1];
            let mut short = TextBuffer::new(&mut short);
            assert_eq!(
                encode_unavailable(fullest(), recovering, &mut short),
                Err(Error::Encoding(bounded::Error::Capacity))
            );
            assert!(short.is_empty());
        }
        Ok(())
    }
}
