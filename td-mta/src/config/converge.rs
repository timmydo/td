//! Rewrites selected profile sources to this release's documented defaults plus
//! a reviewed override policy. Pure source text; no database or serving
//! authority. Candidate preparation still validates the result.
use super::{
    listener::Kind,
    resources::Section,
    syntax::{self, Statement, Value},
    values,
};
use crate::limits::{Limits, ResourceError, ResourcePlan, MAX_MESSAGE_BYTES};
use std::num::NonZeroU32;

/// The policy is a short reviewed file, far below one profile source.
pub const MAX_POLICY_BYTES: usize = 64 * 1024;
const MAX_POLICY_ENTRIES: usize = 1024;
const SESSION_LIMIT: &str = "session_limit";
const PER_PEER_LIMIT: &str = "per_peer_limit";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Capacity,
    /// The policy is not the documented grammar at this one-based line.
    Policy(u32),
    PolicyTooLarge,
    /// The policy names a profile the configuration does not contain.
    PolicyProfile,
    /// A stored source does not parse as the validated grammar it was.
    Source,
    /// Defaults cannot divide one session pool among several SMTP listeners.
    SmtpListeners,
    /// An SMTP listener lacks one of its required limit fields.
    ListenerLimits,
    Plan(ResourceError),
    /// The plan leaves less than one message buffer unassigned.
    Headroom,
}
impl Error {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Capacity => "capacity",
            Self::Policy(_) => "policy-syntax",
            Self::PolicyTooLarge => "policy-too-large",
            Self::PolicyProfile => "policy-unknown-profile",
            Self::Source => "source-syntax",
            Self::SmtpListeners => "several-smtp-listeners",
            Self::ListenerLimits => "smtp-listener-limits",
            Self::Plan(ResourceError::MemoryBudget { .. }) => "memory-budget",
            Self::Plan(_) => "resource-plan",
            Self::Headroom => "message-buffer-headroom",
        }
    }
    /// Whether the policy, not the stored configuration, is at fault.
    pub const fn in_policy(self) -> bool {
        !matches!(
            self,
            Self::Source | Self::SmtpListeners | Self::ListenerLimits | Self::Capacity
        )
    }
}
impl From<td_alloc::Error> for Error {
    fn from(_: td_alloc::Error) -> Self {
        Self::Capacity
    }
}

#[derive(Clone, Copy)]
struct Entry {
    profile: usize,
    section: Section,
    field: &'static str,
    value: u64,
}

/// Reviewed differences from the defaults, keyed by profile name. Only
/// resource stanzas: `[limits "PROFILE"]`, `[disk "PROFILE"]`,
/// `[work "PROFILE"]` and `[network "PROFILE"]` with integer fields.
pub struct Policy {
    profiles: Vec<String>,
    entries: Vec<Entry>,
}

impl Policy {
    pub fn parse(input: &[u8]) -> Result<Self, Error> {
        if input.len() > MAX_POLICY_BYTES {
            return Err(Error::PolicyTooLarge);
        }
        let mut policy = Self {
            profiles: Vec::new(),
            entries: Vec::new(),
        };
        let mut blocks: Vec<(usize, Section)> = Vec::new();
        let mut current = None;
        walk(input, Error::Policy, |line, _, statement| {
            match statement {
                Statement::Empty => {}
                Statement::Section { name, label, .. } => {
                    let section = Section::from_name(name).ok_or(Error::Policy(line))?;
                    let label = label.ok_or(Error::Policy(line))?;
                    values::profile_name(label).map_err(|_| Error::Policy(line))?;
                    let profile = match policy.profiles.iter().position(|p| p == label) {
                        Some(index) => index,
                        None => {
                            td_alloc::push(&mut policy.profiles, td_alloc::string(label)?)?;
                            policy
                                .profiles
                                .len()
                                .checked_sub(1)
                                .ok_or(Error::Capacity)?
                        }
                    };
                    if blocks.contains(&(profile, section)) {
                        return Err(Error::Policy(line));
                    }
                    td_alloc::push(&mut blocks, (profile, section))?;
                    current = Some((profile, section));
                }
                Statement::Assignment { key, value, .. } => {
                    let (profile, section) = current.ok_or(Error::Policy(line))?;
                    let field = section
                        .fields()
                        .iter()
                        .copied()
                        .find(|field| *field == key)
                        .ok_or(Error::Policy(line))?;
                    let Value::Integer(value) = value else {
                        return Err(Error::Policy(line));
                    };
                    if policy.entries.len() >= MAX_POLICY_ENTRIES
                        || policy.entries.iter().any(|e| {
                            e.profile == profile && e.section == section && e.field == field
                        })
                    {
                        return Err(Error::Policy(line));
                    }
                    td_alloc::push(
                        &mut policy.entries,
                        Entry {
                            profile,
                            section,
                            field,
                            value,
                        },
                    )?;
                }
            }
            Ok(())
        })?;
        for index in 0..policy.profiles.len() {
            policy.plan_index(Some(index))?;
        }
        Ok(policy)
    }

    /// Every profile the policy names must exist: a stale override is a
    /// review error, not something to ignore.
    pub fn check_profiles(&self, names: &[&str]) -> Result<(), Error> {
        if self
            .profiles
            .iter()
            .all(|profile| names.contains(&profile.as_str()))
        {
            Ok(())
        } else {
            Err(Error::PolicyProfile)
        }
    }

    fn index(&self, profile: &str) -> Option<usize> {
        self.profiles.iter().position(|p| p == profile)
    }

    /// The defaults plus this profile's `[limits]` overrides, planned. The
    /// plan must fit the budget and leave one maximal message buffer
    /// unassigned (RESOURCES.md, "Connection pool sizing").
    pub fn plan(&self, profile: &str) -> Result<ResourcePlan, Error> {
        self.plan_index(self.index(profile))
    }

    fn plan_index(&self, profile: Option<usize>) -> Result<ResourcePlan, Error> {
        let mut limits = Limits::default();
        for entry in self.of(profile, Section::Limits) {
            let index = Limits::CONFIG_FIELDS
                .iter()
                .position(|field| *field == entry.field)
                .ok_or(Error::Capacity)?;
            let value = usize::try_from(entry.value).map_err(|_| {
                Error::Plan(ResourceError::OutOfRange {
                    field: entry.field,
                    min: 0,
                    max: usize::MAX,
                })
            })?;
            if !limits.set_config(index, value) {
                return Err(Error::Capacity);
            }
        }
        let plan = limits.plan().map_err(Error::Plan)?;
        let unassigned = plan
            .limits()
            .memory_budget_bytes
            .checked_sub(plan.total_bytes())
            .ok_or(Error::Headroom)?;
        if unassigned < MAX_MESSAGE_BYTES {
            return Err(Error::Headroom);
        }
        Ok(plan)
    }

    fn of(&self, profile: Option<usize>, section: Section) -> impl Iterator<Item = &Entry> {
        self.entries
            .iter()
            .filter(move |e| Some(e.profile) == profile && e.section == section)
    }
}

/// One rewritten setting. `None` is the release default: the key is absent.
pub struct Change {
    pub section: &'static str,
    /// The listener stanza's label for listener fields.
    pub listener: Option<String>,
    pub key: &'static str,
    pub before: Option<u64>,
    pub after: Option<u64>,
}

pub struct Converged {
    pub source: Vec<u8>,
    pub changes: Vec<Change>,
    pub plan: ResourcePlan,
}

#[derive(Clone, Copy)]
enum Stanza {
    Start,
    Resource(Section),
    Listener(usize),
    Other,
}

fn stanza(name: &str, label: Option<&str>, listeners: &mut usize) -> Result<Stanza, Error> {
    if let Some(section) = Section::from_name(name) {
        return match label {
            None => Ok(Stanza::Resource(section)),
            Some(_) => Err(Error::Source),
        };
    }
    if name == "listener" {
        let index = *listeners;
        *listeners = index.checked_add(1).ok_or(Error::Source)?;
        return Ok(Stanza::Listener(index));
    }
    Ok(Stanza::Other)
}

/// Remove every resource stanza, append the policy's for `name`, and set
/// the sole SMTP listener's `session_limit` and `per_peer_limit` to the
/// planned pool and per-peer cap. Comments and other lines are kept.
pub fn profile(name: &str, source: &[u8], policy: &Policy) -> Result<Converged, Error> {
    let plan = policy.plan(name)?;
    let profile = policy.index(name);
    // First pass: which listener stanzas are SMTP, by their `kind`.
    let mut labels: Vec<String> = Vec::new();
    let mut smtp: Vec<bool> = Vec::new();
    let mut current = Stanza::Start;
    walk(
        source,
        |_| Error::Source,
        |_, _, statement| {
            match statement {
                Statement::Empty => {}
                Statement::Section { name, label, .. } => {
                    let mut count = labels.len();
                    current = stanza(name, label, &mut count)?;
                    if let Stanza::Listener(_) = current {
                        td_alloc::push(
                            &mut labels,
                            td_alloc::string(label.ok_or(Error::Source)?)?,
                        )?;
                        td_alloc::push(&mut smtp, false)?;
                    }
                }
                Statement::Assignment { key, value, .. } => {
                    if let (Stanza::Listener(index), "kind", Value::Text(kind)) =
                        (current, key, value)
                    {
                        let kind = Kind::from_name(kind).ok_or(Error::Source)?;
                        *smtp.get_mut(index).ok_or(Error::Source)? = kind.is_smtp();
                    }
                }
            }
            Ok(())
        },
    )?;
    if smtp.iter().filter(|smtp| **smtp).count() > 1 {
        return Err(Error::SmtpListeners);
    }
    let sessions = u64::try_from(plan.limits().smtp_sessions).map_err(|_| Error::Capacity)?;
    let per_peer = u64::try_from(plan.limits().smtp_per_peer).map_err(|_| Error::Capacity)?;
    let mut out = td_alloc::with_capacity(source.len().saturating_add(1024))?;
    let mut changes = Vec::new();
    let mut removed: Vec<(Section, &'static str, u64)> = Vec::new();
    let mut seen = [false; 2];
    let mut listeners = 0;
    current = Stanza::Start;
    walk(
        source,
        |_| Error::Source,
        |_, raw, statement| {
            match statement {
                Statement::Section { name, label, .. } => {
                    current = stanza(name, label, &mut listeners)?;
                    if let Stanza::Resource(_) = current {
                        return Ok(());
                    }
                }
                Statement::Assignment {
                    key,
                    value,
                    value_location,
                    ..
                } => match current {
                    Stanza::Resource(section) => {
                        let field = section
                            .fields()
                            .iter()
                            .copied()
                            .find(|field| *field == key)
                            .ok_or(Error::Source)?;
                        let Value::Integer(value) = value else {
                            return Err(Error::Source);
                        };
                        td_alloc::push(&mut removed, (section, field, value))?;
                        return Ok(());
                    }
                    Stanza::Listener(index)
                        if smtp.get(index).copied().unwrap_or(false)
                            && (key == SESSION_LIMIT || key == PER_PEER_LIMIT) =>
                    {
                        let Value::Integer(before) = value else {
                            return Err(Error::Source);
                        };
                        let (field, after, slot) = if key == SESSION_LIMIT {
                            (SESSION_LIMIT, sessions, 0)
                        } else {
                            (PER_PEER_LIMIT, per_peer, 1)
                        };
                        *seen.get_mut(slot).ok_or(Error::Source)? = true;
                        if before != after {
                            // Only the integer token changes; spacing and a
                            // trailing comment stay.
                            let start = usize::from(value_location.column)
                                .checked_sub(1)
                                .ok_or(Error::Source)?;
                            let rest = raw.get(start..).ok_or(Error::Source)?;
                            let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
                            td_alloc::extend_from_slice(
                                &mut out,
                                raw.get(..start).ok_or(Error::Source)?,
                            )?;
                            decimal(after, &mut out)?;
                            td_alloc::extend_from_slice(
                                &mut out,
                                rest.get(digits..).ok_or(Error::Source)?,
                            )?;
                            let label = labels.get(index).ok_or(Error::Source)?;
                            td_alloc::push(
                                &mut changes,
                                Change {
                                    section: "listener",
                                    listener: Some(td_alloc::string(label)?),
                                    key: field,
                                    before: Some(before),
                                    after: Some(after),
                                },
                            )?;
                            return Ok(());
                        }
                    }
                    _ => {}
                },
                Statement::Empty => {}
            }
            td_alloc::extend_from_slice(&mut out, raw)?;
            Ok(())
        },
    )?;
    if smtp.contains(&true) && seen != [true; 2] {
        return Err(Error::ListenerLimits);
    }
    for &(section, field, before) in &removed {
        let after = policy
            .of(profile, section)
            .find(|e| e.field == field)
            .map(|e| e.value);
        if after != Some(before) {
            td_alloc::push(
                &mut changes,
                Change {
                    section: section.name(),
                    listener: None,
                    key: field,
                    before: Some(before),
                    after,
                },
            )?;
        }
    }
    for &section in Section::ALL {
        let mut header = false;
        for entry in policy.of(profile, section) {
            if !removed
                .iter()
                .any(|(s, field, _)| *s == section && *field == entry.field)
            {
                td_alloc::push(
                    &mut changes,
                    Change {
                        section: section.name(),
                        listener: None,
                        key: entry.field,
                        before: None,
                        after: Some(entry.value),
                    },
                )?;
            }
            if !header {
                if out.last().is_some_and(|byte| *byte != b'\n') {
                    td_alloc::push(&mut out, b'\n')?;
                }
                td_alloc::push(&mut out, b'[')?;
                td_alloc::extend_from_slice(&mut out, section.name().as_bytes())?;
                td_alloc::extend_from_slice(&mut out, b"]\n")?;
                header = true;
            }
            td_alloc::extend_from_slice(&mut out, entry.field.as_bytes())?;
            td_alloc::extend_from_slice(&mut out, b" = ")?;
            decimal(entry.value, &mut out)?;
            td_alloc::push(&mut out, b'\n')?;
        }
    }
    // A rewrite that changes no setting keeps the stored bytes, so an
    // unchanged report never publishes a respelled copy.
    let source = if changes.is_empty() {
        td_alloc::copy(source)?
    } else {
        out
    };
    Ok(Converged {
        source,
        changes,
        plan,
    })
}

/// Visits every physical line with its one-based number and statement.
fn walk(
    input: &[u8],
    fail: impl Fn(u32) -> Error,
    mut visit: impl FnMut(u32, &[u8], Statement<'_>) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut scratch = [0; syntax::MAX_STRING_BYTES];
    for (index, raw) in input.split_inclusive(|byte| *byte == b'\n').enumerate() {
        let line = index
            .checked_add(1)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(fail(u32::MAX))?;
        let number = NonZeroU32::new(line).ok_or(fail(line))?;
        let statement = syntax::parse_line(number, raw, &mut scratch).map_err(|_| fail(line))?;
        visit(line, raw, statement)?;
    }
    Ok(())
}

fn decimal(value: u64, out: &mut Vec<u8>) -> Result<(), Error> {
    let mut digits = [0; 20];
    let mut start = digits.len();
    let mut rest = value;
    loop {
        start = start.checked_sub(1).ok_or(Error::Capacity)?;
        let digit = u8::try_from(rest % 10).map_err(|_| Error::Capacity)?;
        *digits.get_mut(start).ok_or(Error::Capacity)? = b'0' + digit;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    td_alloc::extend_from_slice(out, digits.get(start..).ok_or(Error::Capacity)?)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const FRONTEND: &str = "[server]\nhostname = \"mx.example.test\"\n\
    [limits]\nevent_streams = 4\n\
    [listener \"https\"]\nkind = \"https\"\nbind = \"0.0.0.0:443\"\ncertificate = \"public\"\n";
    const RECEIVE: &str = "[server]\nhostname = \"mx.example.test\"\n\
    # Inbound mail.\n\
    [listener \"mx\"]\nkind = \"direct_smtp\"\nbind = \"0.0.0.0:25\"\n\
    server_name = \"mx.example.test\"\ncertificate = \"public\"\n\
    session_limit = 8\nper_peer_limit = 2\n\
    [network]\nsmtp_idle_seconds = 300\n";

    fn text(converged: &Converged) -> &str {
        std::str::from_utf8(&converged.source).unwrap()
    }

    fn summary(changes: &[Change]) -> Vec<String> {
        changes
            .iter()
            .map(|c| {
                format!(
                    "{}{}.{} {:?}->{:?}",
                    c.section,
                    c.listener
                        .as_deref()
                        .map(|l| format!("[{l}]"))
                        .unwrap_or_default(),
                    c.key,
                    c.before,
                    c.after
                )
            })
            .collect()
    }

    #[test]
    fn empty_policy_removes_resource_stanzas_and_sizes_the_listener_to_the_pool() {
        let policy = Policy::parse(b"# No differences.\n\n").unwrap();
        let frontend = profile("frontend", FRONTEND.as_bytes(), &policy).unwrap();
        assert_eq!(
            text(&frontend),
            "[server]\nhostname = \"mx.example.test\"\n\
    [listener \"https\"]\nkind = \"https\"\nbind = \"0.0.0.0:443\"\ncertificate = \"public\"\n"
        );
        assert_eq!(
            summary(&frontend.changes),
            ["limits.event_streams Some(4)->None"]
        );
        assert_eq!(frontend.plan.limits().event_streams, 8);
        let receive = profile("receive", RECEIVE.as_bytes(), &policy).unwrap();
        assert!(text(&receive).contains("# Inbound mail.\n"));
        assert!(text(&receive).contains("session_limit = 32\nper_peer_limit = 2\n"));
        assert!(!text(&receive).contains("[network]"));
        assert_eq!(
            summary(&receive.changes),
            [
                "listener[mx].session_limit Some(8)->Some(32)",
                "network.smtp_idle_seconds Some(300)->None"
            ]
        );
        // Converging again changes nothing.
        for (name, first) in [("frontend", &frontend), ("receive", &receive)] {
            let again = profile(name, &first.source, &policy).unwrap();
            assert_eq!(again.source, first.source);
            assert!(again.changes.is_empty());
        }
    }

    #[test]
    fn policy_values_replace_explicit_ones_and_follow_into_the_listener() {
        let policy = Policy::parse(
            b"[limits \"frontend\"]\nevent_streams = 6\n\
    [limits \"receive\"]\nsmtp_sessions = 16\nsmtp_per_peer = 4\n\
    [network \"receive\"]\nsmtp_idle_seconds = 300\n",
        )
        .unwrap();
        let frontend = profile("frontend", FRONTEND.as_bytes(), &policy).unwrap();
        assert!(
            text(&frontend).ends_with("certificate = \"public\"\n[limits]\nevent_streams = 6\n")
        );
        assert_eq!(
            summary(&frontend.changes),
            ["limits.event_streams Some(4)->Some(6)"]
        );
        let receive = profile("receive", RECEIVE.as_bytes(), &policy).unwrap();
        assert!(text(&receive).contains("session_limit = 16\nper_peer_limit = 4\n"));
        assert!(text(&receive).ends_with(
            "[limits]\nsmtp_sessions = 16\nsmtp_per_peer = 4\n[network]\nsmtp_idle_seconds = 300\n"
        ));
        assert_eq!(
            summary(&receive.changes),
            [
                "listener[mx].session_limit Some(8)->Some(16)",
                "listener[mx].per_peer_limit Some(2)->Some(4)",
                "limits.smtp_sessions None->Some(16)",
                "limits.smtp_per_peer None->Some(4)"
            ]
        );
        for (name, first) in [("frontend", &frontend), ("receive", &receive)] {
            let again = profile(name, &first.source, &policy).unwrap();
            assert_eq!(again.source, first.source);
            assert!(again.changes.is_empty(), "{:?}", summary(&again.changes));
        }
        // A profile the policy does not name takes every default.
        let relay = profile("relay", FRONTEND.as_bytes(), &policy).unwrap();
        assert_eq!(relay.plan.limits().event_streams, 8);
    }

    #[test]
    fn line_endings_comments_and_a_final_unterminated_line_survive() {
        let source = "[server]\r\nhostname = \"mx.example.test\" # name\r\n\
    [listener \"mx\"]\r\nkind = \"direct_smtp\"\r\nper_peer_limit = 1\r\nsession_limit = 1";
        let policy = Policy::parse(b"[limits \"p\"]\nevent_streams = 2").unwrap();
        let converged = profile("p", source.as_bytes(), &policy).unwrap();
        assert_eq!(
            text(&converged),
            "[server]\r\nhostname = \"mx.example.test\" # name\r\n\
    [listener \"mx\"]\r\nkind = \"direct_smtp\"\r\nper_peer_limit = 2\r\nsession_limit = 32\n\
    [limits]\nevent_streams = 2\n"
        );
        let again = profile("p", &converged.source, &policy).unwrap();
        assert_eq!(again.source, converged.source);
    }

    #[test]
    fn only_the_changed_integer_is_rewritten_and_no_change_keeps_the_bytes() {
        let policy = Policy::parse(b"[network \"p\"]\nsmtp_idle_seconds = 300\n").unwrap();
        // Limits before `kind`, indentation and trailing comments.
        let source =
            "[listener \"mx\"]\n  session_limit=8   # tuned\n\tper_peer_limit = 2 # peer\n\
    kind = \"loopback_smtp_fixture\"\n[network]\nsmtp_idle_seconds = 300 # dropped with its line\n\
    [limits]\n[server]\nhostname = \"mx.example.test\"\n";
        let converged = profile("p", source.as_bytes(), &policy).unwrap();
        assert_eq!(
            text(&converged),
            "[listener \"mx\"]\n  session_limit=32   # tuned\n\tper_peer_limit = 2 # peer\n\
    kind = \"loopback_smtp_fixture\"\n[server]\nhostname = \"mx.example.test\"\n\
    [network]\nsmtp_idle_seconds = 300\n"
        );
        assert_eq!(
            summary(&converged.changes),
            ["listener[mx].session_limit Some(8)->Some(32)"]
        );
        // Restated values in another place or spelling are no change: the
        // stored bytes stay as they are.
        let settled = source.replace("session_limit=8", "session_limit=32");
        let unchanged = profile("p", settled.as_bytes(), &policy).unwrap();
        assert!(unchanged.changes.is_empty());
        assert_eq!(unchanged.source, settled.as_bytes());
        let again = profile("p", &converged.source, &policy).unwrap();
        assert!(again.changes.is_empty());
        assert_eq!(again.source, converged.source);
    }

    #[test]
    fn defaults_cannot_divide_one_pool_among_several_smtp_listeners() {
        let policy = Policy::parse(b"").unwrap();
        let two = format!("{RECEIVE}[listener \"gateway\"]\nkind = \"gateway_smtp\"\nsession_limit = 4\nper_peer_limit = 1\n");
        assert_eq!(
            profile("receive", two.as_bytes(), &policy).err(),
            Some(Error::SmtpListeners)
        );
        let missing = RECEIVE.replace("per_peer_limit = 2\n", "");
        assert_eq!(
            profile("receive", missing.as_bytes(), &policy).err(),
            Some(Error::ListenerLimits)
        );
        // An HTTPS listener's fields are never rewritten.
        assert!(profile("frontend", FRONTEND.as_bytes(), &policy).is_ok());
        assert_eq!(
            profile("x", b"[limits \"label\"]\n", &policy).err(),
            Some(Error::Source)
        );
        assert_eq!(
            profile("x", b"[server\n", &policy).err(),
            Some(Error::Source)
        );
    }

    #[test]
    fn policy_grammar_is_closed() {
        for (input, line) in [
            ("[server \"p\"]\n", 1),
            ("[limits]\n", 1),
            ("[limits \"Bad\"]\n", 1),
            ("event_streams = 4\n", 1),
            ("[limits \"p\"]\nunknown = 1\n", 2),
            ("[limits \"p\"]\nevent_streams = \"4\"\n", 2),
            ("[limits \"p\"]\nevent_streams = 4\nevent_streams = 4\n", 3),
            ("[limits \"p\"]\n[disk \"p\"]\n[limits \"p\"]\n", 3),
            ("# fine\n[limits \"p\"]\nsession_limit = 8\n", 3),
            ("[listener \"p\"]\n", 1),
            ("[limits \"p\"]\nevent_streams = 4 junk\n", 2),
            ("[limits \"p\"]\nevent_streams = 99999999999999999999\n", 2),
        ] {
            assert_eq!(
                Policy::parse(input.as_bytes()).err(),
                Some(Error::Policy(line)),
                "{input}"
            );
        }
        assert_eq!(
            Policy::parse(&vec![b'#'; MAX_POLICY_BYTES + 1]).err(),
            Some(Error::PolicyTooLarge)
        );
        let policy = Policy::parse(b"[limits \"a\"]\n[disk \"b\"]\n[limits \"b\"]\n").unwrap();
        assert!(policy.check_profiles(&["a", "b", "c"]).is_ok());
        assert_eq!(
            policy.check_profiles(&["a"]).err(),
            Some(Error::PolicyProfile)
        );
    }

    #[test]
    fn policy_plans_keep_one_message_buffer_unassigned() {
        // RESOURCES.md: the defaults leave about 35.94 MiB unassigned.
        let plan = Policy::parse(b"").unwrap().plan("p").unwrap();
        assert_eq!(
            plan.total_bytes(),
            Limits::default().plan().unwrap().total_bytes()
        );
        assert!(plan.limits().memory_budget_bytes - plan.total_bytes() >= MAX_MESSAGE_BYTES);
        // The compiled maxima fit the budget but spend the last message buffer.
        for field in ["smtp_sessions = 64", "event_streams = 32"] {
            let input = format!("[limits \"p\"]\n{field}\n");
            assert_eq!(
                Policy::parse(input.as_bytes()).err(),
                Some(Error::Headroom),
                "{field}"
            );
        }
        // A larger budget funds them.
        let funded =
            Policy::parse(b"[limits \"p\"]\nsmtp_sessions = 64\nmemory_budget_bytes = 570425344\n")
                .unwrap();
        assert_eq!(funded.plan("p").unwrap().limits().smtp_sessions, 64);
        assert!(matches!(
            Policy::parse(b"[limits \"p\"]\nhttps_connections = 64\n").err(),
            Some(Error::Plan(ResourceError::MemoryBudget { .. }))
        ));
        assert!(matches!(
            Policy::parse(b"[limits \"p\"]\nevent_streams = 33\n").err(),
            Some(Error::Plan(ResourceError::OutOfRange { .. }))
        ));
        assert_eq!(
            Error::Plan(ResourceError::MemoryBudget {
                required: 1,
                available: 0
            })
            .code(),
            "memory-budget"
        );
    }
}
