//! Bounded diagnostic records; these are neither authorization nor a journal.
use crate::ports::TlsVersion;
use crate::{
    bounded::{self, TextBuffer},
    format::Sequence,
    ids::{BootId, SubmissionId},
};
use std::{fmt, fmt::Write, num::NonZeroU64};

pub mod health;
pub mod queue;

pub const SCHEMA_VERSION: u16 = 1;
pub const MAX_EVENT_BYTES: usize = 1024;
pub const MAX_INSPECTION_BYTES: usize = 4096;
pub const MAX_UNTRUSTED_BYTES: usize = 256;

/// Values supplied by the service, never parsed from a peer's diagnostic text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Context {
    pub boot: BootId,
    /// UTC milliseconds since Unix epoch; None means the clock failed.
    pub utc_ms: Option<i64>,
    pub config_generation: u64,
    pub connection: Option<NonZeroU64>,
    pub request: Option<NonZeroU64>,
    pub transaction: Option<Sequence>,
    pub submission: Option<SubmissionId>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Protocol {
    Smtp,
    Jmap,
    Relay,
}
impl Protocol {
    fn code(self) -> &'static str {
        match self {
            Self::Smtp => "smtp",
            Self::Jmap => "jmap",
            Self::Relay => "relay",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Listener {
    Smtp,
    Jmap,
}
impl Listener {
    fn code(self) -> &'static str {
        match self {
            Self::Smtp => "smtp",
            Self::Jmap => "jmap",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
    Info,
    Warning,
    Error,
}
impl Severity {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Security {
    Plain,
    Tls12,
    Tls13,
}
impl Security {
    fn code(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Tls12 => "tls12",
            Self::Tls13 => "tls13",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    Capacity,
    Quota,
    Deadline,
    Storage,
    Configuration,
    Recovery,
}
impl Refusal {
    fn code(self) -> &'static str {
        match self {
            Self::Capacity => "capacity",
            Self::Quota => "quota",
            Self::Deadline => "deadline",
            Self::Storage => "storage",
            Self::Configuration => "configuration",
            Self::Recovery => "recovery",
        }
    }
    fn action(self) -> &'static str {
        match self {
            Self::Capacity | Self::Deadline => "retry_later",
            Self::Quota => "inspect_quota",
            Self::Storage => "inspect_storage",
            Self::Configuration => "check_config",
            Self::Recovery => "inspect_recovery",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Delivery {
    Accepted,
    Retry,
    PermanentFailure,
    Unknown,
}
impl Delivery {
    fn code(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Retry => "retry",
            Self::PermanentFailure => "permanent_failure",
            Self::Unknown => "unknown",
        }
    }
}
/// No free-form strings, addresses, credentials or message fields are accepted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    BootStarted,
    ConfigActivated,
    ListenerReady {
        protocol: Listener,
    },
    ConnectionOpened {
        protocol: Protocol,
        security: Security,
    },
    ConnectionClosed,
    TlsEstablished {
        protocol: Protocol,
        version: TlsVersion,
    },
    TlsFailed {
        protocol: Protocol,
    },
    MailAccepted {
        bytes: u64,
    },
    AdmissionRefused {
        reason: Refusal,
    },
    JournalCommitted,
    CheckpointSelected {
        generation: u64,
    },
    RelayOutcome {
        outcome: Delivery,
    },
    AuthenticationFailed,
    CertificateRenewed {
        expires_utc_ms: i64,
    },
    CertificateRenewalFailed,
    LogSuppressed {
        count: u64,
        saturated: bool,
    },
    LogWriteFailed {
        os_code: Option<i32>,
    },
}
impl Kind {
    pub const fn code(self) -> &'static str {
        match self {
            Self::BootStarted => "boot_started",
            Self::ConfigActivated => "config_activated",
            Self::ListenerReady { .. } => "listener_ready",
            Self::ConnectionOpened { .. } => "connection_opened",
            Self::ConnectionClosed => "connection_closed",
            Self::TlsEstablished { .. } => "tls_established",
            Self::TlsFailed { .. } => "tls_failed",
            Self::MailAccepted { .. } => "mail_accepted",
            Self::AdmissionRefused { .. } => "admission_refused",
            Self::JournalCommitted => "journal_committed",
            Self::CheckpointSelected { .. } => "checkpoint_selected",
            Self::RelayOutcome { .. } => "relay_outcome",
            Self::AuthenticationFailed => "authentication_failed",
            Self::CertificateRenewed { .. } => "certificate_renewed",
            Self::CertificateRenewalFailed => "certificate_renewal_failed",
            Self::LogSuppressed { .. } => "log_suppressed",
            Self::LogWriteFailed { .. } => "log_write_failed",
        }
    }
    pub const fn severity(self) -> Severity {
        match self {
            Self::AdmissionRefused { .. }
            | Self::TlsFailed { .. }
            | Self::AuthenticationFailed
            | Self::CertificateRenewalFailed
            | Self::LogSuppressed { .. }
            | Self::RelayOutcome {
                outcome: Delivery::Retry | Delivery::PermanentFailure | Delivery::Unknown,
            } => Severity::Warning,
            Self::LogWriteFailed { .. } => Severity::Error,
            Self::BootStarted
            | Self::ConfigActivated
            | Self::ListenerReady { .. }
            | Self::ConnectionOpened { .. }
            | Self::ConnectionClosed
            | Self::TlsEstablished { .. }
            | Self::MailAccepted { .. }
            | Self::JournalCommitted
            | Self::CheckpointSelected { .. }
            | Self::RelayOutcome {
                outcome: Delivery::Accepted,
            }
            | Self::CertificateRenewed { .. } => Severity::Info,
        }
    }
    fn action(self) -> Option<&'static str> {
        match self {
            Self::AdmissionRefused { reason } => Some(reason.action()),
            Self::RelayOutcome {
                outcome: Delivery::PermanentFailure | Delivery::Unknown,
            } => Some("inspect_submission"),
            Self::CertificateRenewalFailed => Some("inspect_certificate"),
            Self::LogSuppressed { .. } | Self::LogWriteFailed { .. } => Some("inspect_logging"),
            Self::BootStarted
            | Self::ConfigActivated
            | Self::ListenerReady { .. }
            | Self::ConnectionOpened { .. }
            | Self::ConnectionClosed
            | Self::TlsEstablished { .. }
            | Self::TlsFailed { .. }
            | Self::MailAccepted { .. }
            | Self::JournalCommitted
            | Self::CheckpointSelected { .. }
            | Self::AuthenticationFailed
            | Self::RelayOutcome {
                outcome: Delivery::Accepted | Delivery::Retry,
            }
            | Self::CertificateRenewed { .. } => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Event {
    pub context: Context,
    pub kind: Kind,
}
impl Event {
    /// Appends a whole line or leaves the visible output unchanged.
    pub fn encode(&self, output: &mut TextBuffer<'_>) -> Result<(), bounded::Error> {
        if let Kind::LogSuppressed { count, saturated } = self.kind {
            if saturated != (count == u64::MAX) {
                return Err(bounded::Error::InvalidCount);
            }
        }
        output.format(format_args!("{}", EventJson(self)))
    }
}
struct EventJson<'a>(&'a Event);
impl fmt::Display for EventJson<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let e = self.0;
        prefix(f, "event", &e.context)?;
        write!(
            f,
            ",\"facts\":{{\"code\":\"{}\",\"severity\":\"{}\"",
            e.kind.code(),
            e.kind.severity().code()
        )?;
        match e.kind {
            Kind::TlsFailed { protocol } => write!(f, ",\"protocol\":\"{}\"", protocol.code())?,
            Kind::TlsEstablished { protocol, version } => {
                let security = match version {
                    TlsVersion::V12 => "tls12",
                    TlsVersion::V13 => "tls13",
                };
                write!(
                    f,
                    ",\"protocol\":\"{}\",\"security\":\"{security}\"",
                    protocol.code()
                )?;
            }
            Kind::ListenerReady { protocol } => write!(f, ",\"protocol\":\"{}\"", protocol.code())?,
            Kind::ConnectionOpened { protocol, security } => write!(
                f,
                ",\"protocol\":\"{}\",\"security\":\"{}\"",
                protocol.code(),
                security.code()
            )?,
            Kind::MailAccepted { bytes } => write!(f, ",\"bytes\":{bytes}")?,
            Kind::AdmissionRefused { reason } => write!(f, ",\"reason\":\"{}\"", reason.code())?,
            Kind::CheckpointSelected { generation } => write!(f, ",\"generation\":{generation}")?,
            Kind::RelayOutcome { outcome } => write!(f, ",\"outcome\":\"{}\"", outcome.code())?,
            Kind::CertificateRenewed { expires_utc_ms } => {
                write!(f, ",\"expires_utc_ms\":{expires_utc_ms}")?
            }
            Kind::LogSuppressed { count, saturated } => {
                write!(f, ",\"count\":{count},\"saturated\":{saturated}")?
            }
            Kind::LogWriteFailed { os_code } => {
                f.write_str(",\"os_code\":")?;
                match os_code {
                    Some(v) => write!(f, "{v}")?,
                    None => f.write_str("null")?,
                }
            }
            Kind::BootStarted
            | Kind::ConfigActivated
            | Kind::ConnectionClosed
            | Kind::JournalCommitted
            | Kind::AuthenticationFailed
            | Kind::CertificateRenewalFailed => {}
        }
        f.write_str("},\"recommended_actions\":[")?;
        if let Some(action) = e.kind.action() {
            write!(f, "\"{action}\"")?;
        }
        f.write_str("],\"untrusted\":[]}\n")
    }
}
fn prefix(f: &mut fmt::Formatter<'_>, record: &'static str, c: &Context) -> fmt::Result {
    write!(
        f,
        "{{\"version\":{SCHEMA_VERSION},\"record\":\"{record}\",\"boot_id\":\"{}\",\"utc_ms\":",
        c.boot
    )?;
    match c.utc_ms {
        Some(v) => write!(f, "{v}")?,
        None => f.write_str("null")?,
    }
    write!(f, ",\"config_generation\":{}", c.config_generation)?;
    if let Some(v) = c.connection {
        write!(f, ",\"connection_id\":{v}")?;
    }
    if let Some(v) = c.request {
        write!(f, ",\"request_id\":{v}")?;
    }
    if let Some(v) = c.transaction {
        write!(f, ",\"transaction_sequence\":{}", v.number())?;
    }
    if let Some(v) = c.submission {
        write!(f, ",\"submission_id\":\"{v}\"")?;
    }
    Ok(())
}

/// Provenance labels are trusted schema values; the text is never instructions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    SmtpReply,
    MessageHeader,
    ConfigurationInput,
}
impl Source {
    fn code(self) -> &'static str {
        match self {
            Self::SmtpReply => "smtp_reply",
            Self::MessageHeader => "message_header",
            Self::ConfigurationInput => "configuration_input",
        }
    }
}
/// Only the separately authorized inspection path may expose source text.
/// This type supplies bounds/escaping, not authorization or secret detection.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct UntrustedText<'a> {
    source: Source,
    text: &'a str,
    truncated: bool,
}
impl fmt::Debug for UntrustedText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UntrustedText")
            .field("source", &self.source)
            .field("bytes", &self.text.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}
impl<'a> UntrustedText<'a> {
    /// Uses the input length and at most four UTF-8 boundary checks.
    pub fn new(source: Source, text: &'a str) -> Self {
        let mut length = text.len().min(MAX_UNTRUSTED_BYTES);
        while !text.is_char_boundary(length) {
            length = length.saturating_sub(1);
        }
        Self {
            source,
            text: text.get(..length).unwrap_or_default(),
            truncated: length < text.len(),
        }
    }
    pub fn source(&self) -> Source {
        self.source
    }
    pub fn text(&self) -> &'a str {
        self.text
    }
    pub fn truncated(&self) -> bool {
        self.truncated
    }
    /// Explicit inspection record; never accepted by the default event encoder.
    pub fn encode_inspection(
        &self,
        context: &Context,
        output: &mut TextBuffer<'_>,
    ) -> Result<(), bounded::Error> {
        output.format(format_args!(
            "{}",
            InspectionJson {
                context,
                text: *self
            }
        ))
    }
}
struct InspectionJson<'a> {
    context: &'a Context,
    text: UntrustedText<'a>,
}
impl fmt::Display for InspectionJson<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        prefix(f, "inspection", self.context)?;
        write!(f, ",\"facts\":{{}},\"recommended_actions\":[],\"untrusted\":[{{\"source\":\"{}\",\"truncated\":{},\"text\":", self.text.source.code(), self.text.truncated)?;
        quoted(f, self.text.text)?;
        f.write_str("}]}\n")
    }
}
/// ASCII-only output also escapes terminal controls, bidi and line separators.
fn quoted(f: &mut fmt::Formatter<'_>, text: &str) -> fmt::Result {
    f.write_str("\"")?;
    for ch in text.chars() {
        match ch {
            '"' => f.write_str("\\\"")?,
            '\\' => f.write_str("\\\\")?,
            ' '..='~' => f.write_char(ch)?,
            _ => {
                let mut units = [0; 2];
                for unit in ch.encode_utf16(&mut units) {
                    write!(f, "\\u{unit:04x}")?;
                }
            }
        }
    }
    f.write_str("\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> Context {
        Context {
            boot: BootId::from_bytes([0; 16]),
            utc_ms: Some(0),
            config_generation: 1,
            connection: None,
            request: None,
            transaction: None,
            submission: None,
        }
    }
    pub(super) fn fullest() -> Context {
        Context {
            boot: BootId::from_bytes([255; 16]),
            utc_ms: Some(i64::MIN),
            config_generation: u64::MAX,
            connection: NonZeroU64::new(u64::MAX),
            request: NonZeroU64::new(u64::MAX),
            transaction: Some(Sequence::from_u64(u64::MAX)),
            submission: Some(SubmissionId::from_bytes([255; 16])),
        }
    }
    fn kinds() -> Vec<Kind> {
        let mut result = vec![
            Kind::BootStarted,
            Kind::ConfigActivated,
            Kind::ConnectionClosed,
            Kind::MailAccepted { bytes: u64::MAX },
            Kind::JournalCommitted,
            Kind::CheckpointSelected {
                generation: u64::MAX,
            },
            Kind::AuthenticationFailed,
            Kind::CertificateRenewed {
                expires_utc_ms: i64::MIN,
            },
            Kind::CertificateRenewalFailed,
            Kind::LogSuppressed {
                count: u64::MAX,
                saturated: true,
            },
            Kind::LogSuppressed {
                count: u64::MAX - 1,
                saturated: false,
            },
            Kind::LogWriteFailed {
                os_code: Some(i32::MIN),
            },
            Kind::LogWriteFailed { os_code: None },
        ];
        for protocol in [Protocol::Smtp, Protocol::Jmap, Protocol::Relay] {
            result.push(Kind::TlsFailed { protocol });
            for version in [TlsVersion::V12, TlsVersion::V13] {
                result.push(Kind::TlsEstablished { protocol, version });
            }
            for security in [Security::Plain, Security::Tls12, Security::Tls13] {
                result.push(Kind::ConnectionOpened { protocol, security });
            }
        }
        for protocol in [Listener::Smtp, Listener::Jmap] {
            result.push(Kind::ListenerReady { protocol });
        }
        for reason in [
            Refusal::Capacity,
            Refusal::Quota,
            Refusal::Deadline,
            Refusal::Storage,
            Refusal::Configuration,
            Refusal::Recovery,
        ] {
            result.push(Kind::AdmissionRefused { reason });
        }
        for outcome in [
            Delivery::Accepted,
            Delivery::Retry,
            Delivery::PermanentFailure,
            Delivery::Unknown,
        ] {
            result.push(Kind::RelayOutcome { outcome });
        }
        result
    }
    #[test]
    fn literal_event_schema_and_unknown_time_are_stable() -> Result<(), bounded::Error> {
        let mut bytes = [0; MAX_EVENT_BYTES];
        let mut out = TextBuffer::new(&mut bytes);
        Event {
            context: context(),
            kind: Kind::MailAccepted { bytes: 42 },
        }
        .encode(&mut out)?;
        assert_eq!(out.as_str()?, concat!(
            "{\"version\":1,\"record\":\"event\",\"boot_id\":\"00000000000000000000000000000000\",",
            "\"utc_ms\":0,\"config_generation\":1,\"facts\":{\"code\":\"mail_accepted\",\"severity\":\"info\",\"bytes\":42},",
            "\"recommended_actions\":[],\"untrusted\":[]}\n"));
        out.clear();
        Event {
            context: Context {
                utc_ms: None,
                ..context()
            },
            kind: Kind::LogWriteFailed { os_code: None },
        }
        .encode(&mut out)?;
        assert!(out.as_str()?.contains("\"utc_ms\":null"));
        assert!(out.as_str()?.contains("\"os_code\":null"));
        assert!(out
            .as_str()?
            .contains("\"recommended_actions\":[\"inspect_logging\"]"));
        assert!(!out.as_str()?.contains("transaction_sequence"));
        out.clear();
        Event {
            context: fullest(),
            kind: Kind::JournalCommitted,
        }
        .encode(&mut out)?;
        assert_eq!(out.as_str()?, concat!(
            "{\"version\":1,\"record\":\"event\",\"boot_id\":\"ffffffffffffffffffffffffffffffff\",",
            "\"utc_ms\":-9223372036854775808,\"config_generation\":18446744073709551615,",
            "\"connection_id\":18446744073709551615,\"request_id\":18446744073709551615,",
            "\"transaction_sequence\":18446744073709551615,\"submission_id\":\"ffffffffffffffffffffffffffffffff\",",
            "\"facts\":{\"code\":\"journal_committed\",\"severity\":\"info\"},",
            "\"recommended_actions\":[],\"untrusted\":[]}\n"));
        Ok(())
    }
    #[test]
    fn all_event_variants_fit_and_failed_append_keeps_visible_prefix() -> Result<(), bounded::Error>
    {
        assert!(std::mem::size_of::<Option<Event>>() <= 256);
        for kind in kinds() {
            let event = Event {
                context: fullest(),
                kind,
            };
            let mut bytes = [0; MAX_EVENT_BYTES];
            let mut out = TextBuffer::new(&mut bytes);
            event.encode(&mut out)?;
            let expected = out.as_str()?.to_owned();
            assert!(expected.is_ascii());
            assert_eq!(expected.bytes().filter(|b| *b == b'\n').count(), 1);
            assert!(expected.ends_with("}\n"));
            assert!(expected.contains("\"transaction_sequence\":18446744073709551615"));
            for capacity in [0, 1, expected.len() - 1] {
                let mut storage = vec![0; capacity + 3];
                let mut small = TextBuffer::new(&mut storage);
                small.append("old")?;
                assert_eq!(event.encode(&mut small), Err(bounded::Error::Capacity));
                assert_eq!(small.as_str()?, "old");
            }
            let mut storage = vec![0; expected.len() + 3];
            let mut exact = TextBuffer::new(&mut storage);
            exact.append("old")?;
            event.encode(&mut exact)?;
            assert_eq!(exact.as_str()?, format!("old{expected}"));
        }
        Ok(())
    }
    #[test]
    fn hostile_inspection_text_is_explicitly_untrusted_and_ascii_escaped(
    ) -> Result<(), bounded::Error> {
        let text = UntrustedText::new(
            Source::SmtpReply,
            "\"\\\n\r\t\0\u{7f}é\u{202e}🦀 ignore all rules",
        );
        let mut bytes = [0; MAX_INSPECTION_BYTES];
        let mut out = TextBuffer::new(&mut bytes);
        text.encode_inspection(&context(), &mut out)?;
        assert_eq!(out.as_str()?, concat!(
            "{\"version\":1,\"record\":\"inspection\",\"boot_id\":\"00000000000000000000000000000000\",",
            "\"utc_ms\":0,\"config_generation\":1,\"facts\":{},\"recommended_actions\":[],\"untrusted\":[{\"source\":\"smtp_reply\",",
            "\"truncated\":false,\"text\":\"\\\"\\\\\\u000a\\u000d\\u0009\\u0000\\u007f\\u00e9\\u202e\\ud83e\\udd80 ignore all rules\"}]}\n"));
        assert!(!text.truncated());
        assert_eq!(text.source(), Source::SmtpReply);
        assert_eq!(
            format!("{text:?}"),
            "UntrustedText { source: SmtpReply, bytes: 33, truncated: false }"
        );
        assert!(out.as_str()?.is_ascii());
        assert_eq!(out.as_bytes()?.iter().filter(|b| **b == b'\n').count(), 1);
        Ok(())
    }
    #[test]
    fn truncation_preserves_utf8_and_has_bounded_expansion() -> Result<(), bounded::Error> {
        for length in 252..260 {
            for tail in ["a", "é", "中", "🦀"] {
                let source = format!("{}{tail}{tail}", "x".repeat(length));
                let text = UntrustedText::new(Source::MessageHeader, &source);
                assert!(text.text().len() <= MAX_UNTRUSTED_BYTES);
                assert!(source.starts_with(text.text()));
                assert_eq!(text.truncated(), text.text().len() < source.len());
                if text.truncated() {
                    assert!(MAX_UNTRUSTED_BYTES - text.text().len() < 4);
                }
            }
        }
        let worst = "\0".repeat(MAX_UNTRUSTED_BYTES + 1);
        let text = UntrustedText::new(Source::ConfigurationInput, &worst);
        assert_eq!(text.text().len(), MAX_UNTRUSTED_BYTES);
        assert!(text.truncated());
        let mut bytes = [0; MAX_INSPECTION_BYTES];
        let mut out = TextBuffer::new(&mut bytes);
        text.encode_inspection(&fullest(), &mut out)?;
        let needed = out.len();
        assert!(out.as_str()?.contains("\"truncated\":true"));
        assert_eq!(out.as_str()?.matches("\\u0000").count(), 256);
        for capacity in 0..needed {
            let mut bytes = vec![0; capacity];
            let mut out = TextBuffer::new(&mut bytes);
            assert_eq!(
                text.encode_inspection(&fullest(), &mut out),
                Err(bounded::Error::Capacity)
            );
            assert!(out.is_empty());
        }
        Ok(())
    }
    // Independent v1 contract oracle; a new variant requires an explicit policy.
    fn policy(kind: Kind) -> (&'static str, &'static str, &'static str) {
        match kind {
            Kind::BootStarted => ("boot_started", "info", "[]"),
            Kind::ConfigActivated => ("config_activated", "info", "[]"),
            Kind::ListenerReady { .. } => ("listener_ready", "info", "[]"),
            Kind::ConnectionOpened { .. } => ("connection_opened", "info", "[]"),
            Kind::ConnectionClosed => ("connection_closed", "info", "[]"),
            Kind::TlsEstablished { .. } => ("tls_established", "info", "[]"),
            Kind::TlsFailed { .. } => ("tls_failed", "warning", "[]"),
            Kind::MailAccepted { .. } => ("mail_accepted", "info", "[]"),
            Kind::AdmissionRefused { reason } => (
                "admission_refused",
                "warning",
                match reason {
                    Refusal::Capacity | Refusal::Deadline => "[\"retry_later\"]",
                    Refusal::Quota => "[\"inspect_quota\"]",
                    Refusal::Storage => "[\"inspect_storage\"]",
                    Refusal::Configuration => "[\"check_config\"]",
                    Refusal::Recovery => "[\"inspect_recovery\"]",
                },
            ),
            Kind::JournalCommitted => ("journal_committed", "info", "[]"),
            Kind::CheckpointSelected { .. } => ("checkpoint_selected", "info", "[]"),
            Kind::RelayOutcome {
                outcome: Delivery::Accepted,
            } => ("relay_outcome", "info", "[]"),
            Kind::RelayOutcome {
                outcome: Delivery::Retry,
            } => ("relay_outcome", "warning", "[]"),
            Kind::RelayOutcome {
                outcome: Delivery::PermanentFailure | Delivery::Unknown,
            } => ("relay_outcome", "warning", "[\"inspect_submission\"]"),
            Kind::AuthenticationFailed => ("authentication_failed", "warning", "[]"),
            Kind::CertificateRenewed { .. } => ("certificate_renewed", "info", "[]"),
            Kind::CertificateRenewalFailed => (
                "certificate_renewal_failed",
                "warning",
                "[\"inspect_certificate\"]",
            ),
            Kind::LogSuppressed { .. } => ("log_suppressed", "warning", "[\"inspect_logging\"]"),
            Kind::LogWriteFailed { .. } => ("log_write_failed", "error", "[\"inspect_logging\"]"),
        }
    }
    #[test]
    fn version_one_codes_severity_and_actions_are_pinned() -> Result<(), bounded::Error> {
        for kind in kinds() {
            let mut bytes = [0; MAX_EVENT_BYTES];
            let mut out = TextBuffer::new(&mut bytes);
            Event {
                context: context(),
                kind,
            }
            .encode(&mut out)?;
            let (code, severity, actions) = policy(kind);
            assert!(out.as_str()?.contains(&format!(
                "\"facts\":{{\"code\":\"{code}\",\"severity\":\"{severity}\""
            )));
            assert!(out.as_str()?.contains(&format!(
                "\"recommended_actions\":{actions},\"untrusted\":[]"
            )));
        }
        let mut bytes = [0; MAX_EVENT_BYTES];
        let mut out = TextBuffer::new(&mut bytes);
        out.append("old")?;
        for (count, saturated) in [(5, true), (u64::MAX, false)] {
            assert_eq!(
                Event {
                    context: context(),
                    kind: Kind::LogSuppressed { count, saturated }
                }
                .encode(&mut out),
                Err(bounded::Error::InvalidCount)
            );
            assert_eq!(out.as_str()?, "old");
        }
        Ok(())
    }
    #[test]
    fn tls_upgrade_and_failure_have_distinct_correlated_facts() -> Result<(), bounded::Error> {
        let c = Context {
            connection: NonZeroU64::new(7),
            ..context()
        };
        for (kind, facts) in [
            (Kind::ConnectionOpened { protocol: Protocol::Smtp, security: Security::Plain }, "{\"code\":\"connection_opened\",\"severity\":\"info\",\"protocol\":\"smtp\",\"security\":\"plain\"}"),
            (Kind::TlsEstablished { protocol: Protocol::Smtp, version: TlsVersion::V12 }, "{\"code\":\"tls_established\",\"severity\":\"info\",\"protocol\":\"smtp\",\"security\":\"tls12\"}"),
            (Kind::TlsEstablished { protocol: Protocol::Smtp, version: TlsVersion::V13 }, "{\"code\":\"tls_established\",\"severity\":\"info\",\"protocol\":\"smtp\",\"security\":\"tls13\"}"),
            (Kind::TlsFailed { protocol: Protocol::Smtp }, "{\"code\":\"tls_failed\",\"severity\":\"warning\",\"protocol\":\"smtp\"}"),
        ] {
            let mut bytes = [0; MAX_EVENT_BYTES];
            let mut out = TextBuffer::new(&mut bytes);
            Event { context: c, kind }.encode(&mut out)?;
            assert!(out.as_str()?.contains(&format!("\"connection_id\":7,\"facts\":{facts},\"recommended_actions\":[]")));
        }
        Ok(())
    }
}
