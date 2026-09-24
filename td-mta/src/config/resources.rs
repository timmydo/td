//! Scalar resource stanza builder; this is not a complete configuration checker.
use super::syntax::{Location, Value};
use crate::{admission, limits};
use std::fmt;

const MAX_FIELDS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Section {
    Limits,
    Disk,
    Work,
    Network,
}
impl Section {
    pub const ALL: [Self; 4] = [Self::Limits, Self::Disk, Self::Work, Self::Network];
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|section| section.name() == name)
    }
    pub const fn name(self) -> &'static str {
        match self {
            Self::Limits => "limits",
            Self::Disk => "disk",
            Self::Work => "work",
            Self::Network => "network",
        }
    }
    pub const fn fields(self) -> &'static [&'static str] {
        match self {
            Self::Limits => limits::Limits::CONFIG_FIELDS,
            Self::Disk => admission::DiskLimits::CONFIG_FIELDS,
            Self::Work => admission::WorkLimits::CONFIG_FIELDS,
            Self::Network => admission::timers::NetworkLimits::CONFIG_FIELDS,
        }
    }
    const fn index(self) -> usize {
        match self {
            Self::Limits => 0,
            Self::Disk => 1,
            Self::Work => 2,
            Self::Network => 3,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    DuplicateSection,
    DuplicateField,
    UnknownField,
    ExpectedInteger,
    IntegerWidth,
    NoSection,
    SchemaCapacity,
    OutOfRange,
    ResourcePlan,
    AdmissionPlan,
    TimeoutPlan,
}
impl Code {
    pub const fn name(self) -> &'static str {
        match self {
            Self::DuplicateSection => "config_duplicate_section",
            Self::DuplicateField => "config_duplicate_field",
            Self::UnknownField => "config_unknown_field",
            Self::ExpectedInteger => "config_expected_integer",
            Self::IntegerWidth => "config_integer_width",
            Self::NoSection => "config_no_resource_section",
            Self::SchemaCapacity => "config_resource_schema_capacity",
            Self::OutOfRange => "config_out_of_range",
            Self::ResourcePlan => "config_resource_plan",
            Self::AdmissionPlan => "config_admission_plan",
            Self::TimeoutPlan => "config_timeout_plan",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Problem {
    Resource(limits::ResourceError),
    Admission(admission::Error),
}
impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resource(e) => e.fmt(f),
            Self::Admission(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for Problem {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Resource(e) => e.source(),
            Self::Admission(e) => e.source(),
        }
    }
}
/// Fields and planner problems contain only trusted static names and numbers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error {
    pub code: Code,
    pub location: Option<Location>,
    pub previous: Option<Location>,
    pub field: Option<&'static str>,
    pub problem: Option<Problem>,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code.name())?;
        if let Some(location) = self.location {
            write!(
                f,
                " at line {}, byte column {}",
                location.line, location.column
            )?;
        }
        Ok(())
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.problem.as_ref().map(|p| p as &dyn std::error::Error)
    }
}
fn error(code: Code, location: Option<Location>) -> Error {
    Error {
        code,
        location,
        previous: None,
        field: None,
        problem: None,
    }
}
#[derive(Clone, Copy)]
struct Seen {
    section: Option<Location>,
    fields: [Option<Location>; MAX_FIELDS],
}
impl Seen {
    const EMPTY: Self = Self {
        section: None,
        fields: [None; MAX_FIELDS],
    };
}

/// Caller owns whole-file EOF, schema version and all non-resource sections.
/// On the first failure, discard the candidate; later operations return it.
pub struct Builder {
    limits: limits::Limits,
    disk: admission::DiskLimits,
    work: admission::WorkLimits,
    network: admission::timers::NetworkLimits,
    seen: [Seen; 4],
    failure: Option<Error>,
}
impl Default for Builder {
    fn default() -> Self {
        Self {
            limits: limits::Limits::default(),
            disk: admission::DiskLimits::default(),
            work: admission::WorkLimits::default(),
            network: admission::timers::NetworkLimits::default(),
            seen: [Seen::EMPTY; 4],
            failure: None,
        }
    }
}
impl Builder {
    fn record<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(e) = result {
            self.failure = Some(e);
        }
        result
    }
    pub fn begin(&mut self, section: Section, location: Location) -> Result<(), Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = self.begin_inner(section, location);
        self.record(result)
    }
    fn begin_inner(&mut self, section: Section, location: Location) -> Result<(), Error> {
        if section.fields().len() > MAX_FIELDS {
            return Err(error(Code::SchemaCapacity, Some(location)));
        }
        let seen = self
            .seen
            .get_mut(section.index())
            .ok_or(error(Code::SchemaCapacity, Some(location)))?;
        if let Some(previous) = seen.section {
            let mut e = error(Code::DuplicateSection, Some(location));
            e.previous = Some(previous);
            return Err(e);
        }
        seen.section = Some(location);
        Ok(())
    }
    /// The outer parser supplies its current section; no second selection is kept.
    pub fn assign(
        &mut self,
        section: Section,
        key: &str,
        value: Value<'_>,
        location: Location,
    ) -> Result<(), Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = self.assign_inner(section, key, value, location);
        self.record(result)
    }
    fn assign_inner(
        &mut self,
        section: Section,
        key: &str,
        value: Value<'_>,
        location: Location,
    ) -> Result<(), Error> {
        if self
            .seen
            .get(section.index())
            .and_then(|seen| seen.section)
            .is_none()
        {
            return Err(error(Code::NoSection, Some(location)));
        }
        let index = section
            .fields()
            .iter()
            .position(|name| *name == key)
            .ok_or(error(Code::UnknownField, Some(location)))?;
        let field = section
            .fields()
            .get(index)
            .copied()
            .ok_or(error(Code::SchemaCapacity, Some(location)))?;
        let seen = self
            .seen
            .get_mut(section.index())
            .and_then(|s| s.fields.get_mut(index))
            .ok_or(error(Code::SchemaCapacity, Some(location)))?;
        if let Some(previous) = *seen {
            let mut e = error(Code::DuplicateField, Some(location));
            e.previous = Some(previous);
            e.field = Some(field);
            return Err(e);
        }
        let Value::Integer(value) = value else {
            let mut e = error(Code::ExpectedInteger, Some(location));
            e.field = Some(field);
            return Err(e);
        };
        let set = match section {
            Section::Limits => self.limits.set_config(
                index,
                usize::try_from(value).map_err(|_| {
                    let mut e = error(Code::IntegerWidth, Some(location));
                    e.field = Some(field);
                    e
                })?,
            ),
            Section::Disk => self.disk.set_config(index, value),
            Section::Work => self.work.set_config(index, value),
            Section::Network => self.network.set_config(index, value),
        };
        if !set {
            return Err(error(Code::SchemaCapacity, Some(location)));
        }
        *seen = Some(location);
        Ok(())
    }
    fn field_location(&self, section: Section, field: &str) -> Option<Location> {
        let index = section.fields().iter().position(|name| *name == field)?;
        *self.seen.get(section.index())?.fields.get(index)?
    }
    pub fn finish(self, views: admission::ViewMode) -> Result<Validated, Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let resources = self.limits.plan().map_err(|problem| {
            let mut e = error(Code::ResourcePlan, None);
            if let limits::ResourceError::OutOfRange { field, .. } = problem {
                e.code = Code::OutOfRange;
                e.location = self.field_location(Section::Limits, field);
                e.field = Some(field);
            }
            e.problem = Some(Problem::Resource(problem));
            e
        })?;
        let admission = self
            .disk
            .plan(&resources, self.work, views)
            .map_err(|problem| {
                let mut e = error(Code::AdmissionPlan, None);
                if let admission::Error::Range { field, .. } = problem {
                    e.code = Code::OutOfRange;
                    e.location = self
                        .field_location(Section::Disk, field)
                        .or_else(|| self.field_location(Section::Work, field));
                    e.field = Some(field);
                }
                e.problem = Some(Problem::Admission(problem));
                e
            })?;
        let timeouts = self.network.plan(&admission).map_err(|problem| {
            let mut e = error(Code::TimeoutPlan, None);
            if let admission::Error::Range { field, .. } = problem {
                e.code = Code::OutOfRange;
                e.location = self.field_location(Section::Network, field);
                e.field = Some(field);
            }
            e.problem = Some(Problem::Admission(problem));
            e
        })?;
        Ok(Validated {
            resources,
            admission,
            timeouts,
        })
    }
}
/// Validated resource settings only; never authority to open files or serve mail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Validated {
    resources: limits::ResourcePlan,
    admission: admission::Plan,
    timeouts: admission::timers::TimeoutPlan,
}
impl Validated {
    pub fn timeouts(&self) -> &admission::timers::TimeoutPlan {
        &self.timeouts
    }
    pub fn resources(&self) -> &limits::ResourcePlan {
        &self.resources
    }
    pub fn admission(&self) -> &admission::Plan {
        &self.admission
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::config::syntax::{parse_line, Statement};
    use std::num::NonZeroU32;
    fn location(line: u32) -> Location {
        Location {
            line: NonZeroU32::new(line).unwrap(),
            column: 1,
        }
    }
    #[test]
    fn empty_builder_uses_existing_default_plans() {
        let actual = Builder::default()
            .finish(admission::ViewMode::OnlineBackground)
            .unwrap();
        let resources = limits::Limits::default().plan().unwrap();
        let admission = admission::DiskLimits::default()
            .plan(
                &resources,
                admission::WorkLimits::default(),
                admission::ViewMode::OnlineBackground,
            )
            .unwrap();
        let timeouts = admission::timers::NetworkLimits::default()
            .plan(&admission)
            .unwrap();
        assert_eq!(actual.resources(), &resources);
        assert_eq!(actual.admission(), &admission);
        assert_eq!(actual.timeouts(), &timeouts);
        assert!(std::mem::size_of::<Builder>() <= 4096);
    }
    #[test]
    fn decoded_statements_reach_all_three_plans() {
        let lines = [
            "[limits]",
            "message_bytes=67108864",
            "[disk]",
            "body_bytes=8589934592",
            "[work]",
            "request_seconds=600",
            "[network]",
            "minimum_rate=32768",
        ];
        let mut scratch = [0; 4096];
        let mut builder = Builder::default();
        let mut current = None;
        for (i, line) in lines.iter().enumerate() {
            let number = NonZeroU32::new(u32::try_from(i + 1).unwrap()).unwrap();
            match parse_line(number, line.as_bytes(), &mut scratch).unwrap() {
                Statement::Section {
                    location,
                    name,
                    label: None,
                } => {
                    let section = Section::from_name(name).unwrap();
                    current = Some(section);
                    builder.begin(section, location).unwrap();
                }
                Statement::Assignment {
                    key,
                    value,
                    location,
                    ..
                } => builder
                    .assign(current.unwrap(), key, value, location)
                    .unwrap(),
                other => assert_eq!(other, Statement::Empty),
            }
        }
        let actual = builder
            .finish(admission::ViewMode::OnlineBackground)
            .unwrap();
        assert_eq!(actual.resources().limits().message_bytes, 67108864);
        assert_eq!(actual.admission().disk().body_bytes, 8589934592);
        assert_eq!(actual.admission().work().request_seconds, 600);
        assert_eq!(actual.timeouts().execution_seconds(), 600);
        assert_eq!(actual.timeouts().limits().minimum_rate, 32768);
        assert_eq!(actual.resources().total_bytes(), 64_464_128);
    }
    #[test]
    fn duplicates_keep_first_location_and_poison_candidate() {
        let mut b = Builder::default();
        b.begin(Section::Limits, location(1)).unwrap();
        b.assign(
            Section::Limits,
            "message_bytes",
            Value::Integer(67108864),
            location(2),
        )
        .unwrap();
        let e = b
            .assign(
                Section::Limits,
                "message_bytes",
                Value::Integer(33554432),
                location(3),
            )
            .unwrap_err();
        assert_eq!(e.code, Code::DuplicateField);
        assert_eq!(e.location, Some(location(3)));
        assert_eq!(e.previous, Some(location(2)));
        assert_eq!(e.field, Some("message_bytes"));
        assert_eq!(b.begin(Section::Work, location(4)), Err(e));
        assert_eq!(b.finish(admission::ViewMode::OnlineBackground), Err(e));
        let mut b = Builder::default();
        b.begin(Section::Disk, location(1)).unwrap();
        b.begin(Section::Work, location(2)).unwrap();
        let e = b.begin(Section::Disk, location(3)).unwrap_err();
        assert_eq!(e.code, Code::DuplicateSection);
        assert_eq!(e.previous, Some(location(1)));
        assert_eq!(
            b.assign(
                Section::Work,
                "request_seconds",
                Value::Integer(600),
                location(4)
            ),
            Err(e)
        );
        assert_eq!(b.finish(admission::ViewMode::OnlineBackground), Err(e));
    }
    #[test]
    fn unknown_type_and_section_errors_do_not_echo_input() {
        for section in Section::ALL {
            let mut b = Builder::default();
            b.begin(section, location(1)).unwrap();
            let e = b
                .assign(
                    section,
                    "private_fixture",
                    Value::Text("secret_fixture"),
                    location(2),
                )
                .unwrap_err();
            assert_eq!(e.code, Code::UnknownField);
            assert!(!format!("{e} {e:?}").contains("fixture"));
            assert_eq!(
                b.assign(section, section.fields()[0], Value::Integer(1), location(3)),
                Err(e)
            );
            let mut b = Builder::default();
            b.begin(section, location(1)).unwrap();
            let e = b
                .assign(
                    section,
                    section.fields()[0],
                    Value::Text("secret_fixture"),
                    location(2),
                )
                .unwrap_err();
            assert_eq!(e.code, Code::ExpectedInteger);
            assert!(!format!("{e} {e:?}").contains("fixture"));
        }
        let mut b = Builder::default();
        let e = b
            .assign(
                Section::Limits,
                "message_bytes",
                Value::Integer(1),
                location(2),
            )
            .unwrap_err();
        assert_eq!(e.code, Code::NoSection);
    }
    #[test]
    fn range_errors_pin_assignment_but_cross_field_errors_do_not_blame_one_line() {
        for (section, field, value, code) in [
            (Section::Limits, "smtp_sessions", 0, Code::OutOfRange),
            (Section::Disk, "body_bytes", 0, Code::OutOfRange),
            (Section::Work, "request_seconds", 0, Code::OutOfRange),
            (Section::Network, "minimum_rate", 0, Code::OutOfRange),
        ] {
            let mut b = Builder::default();
            b.begin(section, location(1)).unwrap();
            b.assign(section, field, Value::Integer(value), location(2))
                .unwrap();
            let e = b.finish(admission::ViewMode::OnlineBackground).unwrap_err();
            assert_eq!(e.code, code);
            assert_eq!(e.field, Some(field));
            assert_eq!(e.location, Some(location(2)));
            assert!(e.problem.is_some());
            assert!(std::error::Error::source(&e).is_some());
        }
        let mut b = Builder::default();
        b.begin(Section::Limits, location(1)).unwrap();
        b.assign(
            Section::Limits,
            "memory_budget_bytes",
            Value::Integer(1024),
            location(2),
        )
        .unwrap();
        let e = b.finish(admission::ViewMode::OnlineBackground).unwrap_err();
        assert_eq!(e.code, Code::ResourcePlan);
        assert_eq!(e.location, None);
        let source = std::error::Error::source(&e).unwrap();
        assert!(source.source().is_none());
        assert!(source.to_string().contains("1024"));
        assert!(matches!(
            e.problem,
            Some(Problem::Resource(
                limits::ResourceError::MemoryBudget { .. }
            ))
        ));
        let mut b = Builder::default();
        b.begin(Section::Limits, location(1)).unwrap();
        b.assign(
            Section::Limits,
            "storage_views",
            Value::Integer(1),
            location(2),
        )
        .unwrap();
        let e = b.finish(admission::ViewMode::OnlineBackground).unwrap_err();
        assert_eq!(e.code, Code::AdmissionPlan);
        assert_eq!(e.location, None);
    }
    #[test]
    fn view_mode_is_explicit_and_no_integer_is_truncated() {
        let mut b = Builder::default();
        b.begin(Section::Limits, location(1)).unwrap();
        b.assign(
            Section::Limits,
            "storage_views",
            Value::Integer(1),
            location(2),
        )
        .unwrap();
        assert_eq!(
            b.finish(admission::ViewMode::ForegroundOnly)
                .unwrap()
                .admission()
                .view_mode(),
            admission::ViewMode::ForegroundOnly
        );
        let mut b = Builder::default();
        b.begin(Section::Limits, location(1)).unwrap();
        let result = b.assign(
            Section::Limits,
            "smtp_sessions",
            Value::Integer(u64::MAX),
            location(2),
        );
        // The narrowing refusal executes only on targets narrower than u64.
        if usize::BITS < 64 {
            let e = result.unwrap_err();
            assert_eq!(e.code, Code::IntegerWidth);
            assert_eq!(e.field, Some("smtp_sessions"));
        } else {
            result.unwrap();
            assert_eq!(
                b.finish(admission::ViewMode::OnlineBackground)
                    .unwrap_err()
                    .code,
                Code::OutOfRange
            );
        }
    }
    #[test]
    fn version_one_field_vocabulary_is_fixed() {
        assert_eq!(
            Section::Limits.fields(),
            &[
                "smtp_sessions",
                "smtp_per_peer",
                "https_connections",
                "tls_handshakes",
                "event_streams",
                "body_jobs",
                "storage_views",
                "message_bytes",
                "header_bytes",
                "mime_depth",
                "mime_parts",
                "smtp_recipients",
                "json_bytes",
                "json_methods",
                "json_depth",
                "json_tokens",
                "objects_per_method",
                "query_page",
                "index_cache_bytes",
                "upload_disk_bytes",
                "upload_expiry_seconds",
                "queue_disk_bytes",
                "queue_submissions",
                "sort_disk_bytes",
                "log_file_bytes",
                "retained_logs",
                "memory_budget_bytes"
            ]
        );
        assert_eq!(
            Section::Disk.fields(),
            &[
                "body_bytes",
                "body_files",
                "live_metadata_bytes",
                "checkpoint_bytes",
                "response_bytes",
                "response_total_bytes",
                "cache_bytes",
                "cold_bytes",
                "free_bytes",
                "free_inodes"
            ]
        );
        assert_eq!(
            Section::Work.fields(),
            &[
                "foreground_seconds",
                "foreground_io_bytes",
                "foreground_records",
                "changes_seconds",
                "changes_io_bytes",
                "changes_records",
                "request_seconds",
                "commit_seconds",
                "commit_io_bytes",
                "commit_records",
                "checkpoint_seconds",
                "checkpoint_io_bytes",
                "gc_drain_seconds",
                "gc_seconds",
                "gc_io_bytes",
                "gc_records",
                "gc_unlinks",
                "backup_seconds",
                "backup_io_bytes",
                "admission_seconds"
            ]
        );
        assert_eq!(
            Section::Network.fields(),
            &[
                "handshake_seconds",
                "dns_seconds",
                "dial_seconds",
                "header_idle_seconds",
                "header_seconds",
                "body_idle_seconds",
                "response_idle_seconds",
                "keepalive_seconds",
                "event_stall_seconds",
                "smtp_idle_seconds",
                "smtp_data_seconds",
                "command_seconds",
                "data_init_seconds",
                "data_block_seconds",
                "final_reply_seconds",
                "migration_idle_seconds",
                "migration_minimum_seconds",
                "transfer_minimum_seconds",
                "transfer_base_seconds",
                "minimum_rate"
            ]
        );
        for section in Section::ALL {
            assert!(section.fields().len() <= MAX_FIELDS);
            for (index, field) in section.fields().iter().enumerate() {
                assert_eq!(
                    section.fields().iter().position(|name| name == field),
                    Some(index)
                );
            }
        }
    }
    #[test]
    fn resource_diagnostic_names_are_fixed() {
        let cases = [
            (Code::DuplicateSection, "config_duplicate_section"),
            (Code::DuplicateField, "config_duplicate_field"),
            (Code::UnknownField, "config_unknown_field"),
            (Code::ExpectedInteger, "config_expected_integer"),
            (Code::IntegerWidth, "config_integer_width"),
            (Code::NoSection, "config_no_resource_section"),
            (Code::SchemaCapacity, "config_resource_schema_capacity"),
            (Code::OutOfRange, "config_out_of_range"),
            (Code::ResourcePlan, "config_resource_plan"),
            (Code::AdmissionPlan, "config_admission_plan"),
            (Code::TimeoutPlan, "config_timeout_plan"),
        ];
        for (code, name) in cases {
            assert_eq!(code.name(), name);
        }
    }
    #[test]
    fn fixed_fields_cross_section_keys_and_noninteger_values_are_refused() {
        for key in [
            "outbound_deliveries",
            "journal_bytes",
            "journal_operations",
            "frame_bytes",
            "frame_operations",
            "body_bytes",
        ] {
            let mut b = Builder::default();
            b.begin(Section::Limits, location(1)).unwrap();
            assert_eq!(
                b.assign(Section::Limits, key, Value::Integer(1), location(2))
                    .unwrap_err()
                    .code,
                Code::UnknownField
            );
        }
        for section in Section::ALL {
            for value in [
                Value::Boolean(true),
                Value::Boolean(false),
                Value::Text("123"),
            ] {
                let mut b = Builder::default();
                b.begin(section, location(1)).unwrap();
                let key = section.fields()[0];
                let e = b.assign(section, key, value, location(2)).unwrap_err();
                assert_eq!(e.code, Code::ExpectedInteger);
                assert_eq!(e.field, Some(key));
                assert_eq!(b.finish(admission::ViewMode::OnlineBackground), Err(e));
            }
        }
        for field in Section::Disk.fields() {
            assert!(
                !Section::Work.fields().contains(field),
                "ambiguous admission field: {field}"
            );
        }
        for section in Section::ALL {
            assert_eq!(Section::from_name(section.name()), Some(section));
        }
        assert_eq!(Section::from_name("LIMITS"), None);
        assert_eq!(Section::from_name("unknown"), None);
    }
}
