//! `/etc/td-svc.conf` — the unit table.
//!
//! Parsing collects diagnostics rather than aborting, exactly as td-init's
//! `parse_inittab` does: a machine that refuses to boot over one bad stanza
//! cannot be repaired from its own console. `td-svc check` is where a bad
//! table is meant to be fatal, and the image build runs it.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Runs to completion. Ready when it exits 0.
    Oneshot,
    /// Runs until it stops. Ready when spawned, or when `ready=` succeeds.
    Daemon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    Always,
    OnFailure,
    Never,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub name: String,
    pub kind: Kind,
    pub argv: Vec<String>,
    /// Ordering only. A failed dependency does NOT skip these — see `requires`.
    pub after: Vec<String>,
    /// Strict dependency: a failure here skips this unit. Opt-in, and per
    /// DESIGN.md I5 never valid on a `tty=` unit.
    pub requires: Vec<String>,
    pub restart: Restart,
    /// The terminal this unit owns, if any.
    pub tty: Option<String>,
    /// Where this unit's output is captured, if anywhere.
    pub log: Option<String>,
    /// `console=yes`: copy captured lines to `/dev/console` as well. Only
    /// meaningful with `log=`, and refused without it.
    pub console: bool,
    pub timeout: Option<Duration>,
    pub ready: Vec<String>,
    pub ready_timeout: Duration,
    pub stop_timeout: Duration,
    /// Which cgroup this unit's processes are accounted in.
    pub cgroup: Cgroup,
    /// What this unit's own leaf bounds. Refused unless `cgroup=service`, so a
    /// limit is never written where the unit's processes will not be.
    pub limits: Limits,
}

/// Where a unit's processes are accounted.
///
/// A unit that hands its process to another cgroup cannot be bounded by one of
/// its own, and the table is where a reader looks to find out which is which —
/// so it is declared rather than guessed from the `exec=` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Cgroup {
    /// Its own leaf under `/sys/fs/cgroup/system`, created before it starts.
    #[default]
    Service,
    /// The uid-1000 session leaf. `td-login` joins it before dropping
    /// privilege, so the process leaves any leaf td-svc might have made — which
    /// is why declaring this refuses limits rather than writing inert ones.
    Session,
}

/// The controls written into a unit's leaf before it starts.
///
/// `None` is "unbounded", not "default": cgroup v2's own default is `max`, and
/// writing a number nobody chose is how a service acquires a limit its author
/// never reasoned about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Limits {
    /// `memory.max`, in bytes.
    pub memory_max: Option<u64>,
    /// `pids.max`.
    pub pids_max: Option<u64>,
    /// `cpu.weight`, 1..=10000. A share under contention, never a ceiling.
    pub cpu_weight: Option<u32>,
}

impl Limits {
    pub fn is_empty(&self) -> bool {
        self.memory_max.is_none() && self.pids_max.is_none() && self.cpu_weight.is_none()
    }
}

impl Unit {
    /// The path component this unit's leaf is named by, or `None` when it has
    /// no leaf of its own.
    ///
    /// The name is already `[A-Za-z0-9_-]` with no leading `-` and no `.`, so
    /// what this returns is a single path component that cannot collide with a
    /// cgroup interface file. It is re-checked here anyway: this is the value
    /// that becomes a directory under `/sys/fs/cgroup`, and the check that
    /// matters is the one next to the use.
    pub fn cgroup_leaf_name(&self) -> Option<&str> {
        if self.cgroup != Cgroup::Service
            || !is_safe_component(&self.name)
            || self.name.contains('.')
            || self.name.len() > MAX_LEAF_NAME
        {
            return None;
        }
        Some(&self.name)
    }
}

/// The longest name cgroupfs will accept as a directory, so the longest a unit
/// may be named. Exceeding it fails with `ENAMETOOLONG` at boot; refused at
/// `check` time instead.
const MAX_LEAF_NAME: usize = 255;

/// One path component: not empty, not `.` or `..`, and no separator.
fn is_safe_component(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}

pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Every oneshot gets a deadline whether or not it declares one.
///
/// Without a default, a oneshot that HANGS never settles, and everything
/// ordered after it waits forever — the console included. That is I5's worst
/// outcome reached at runtime, from a table `check` passes as clean, with none
/// of the three console defences engaged: they all reason about the graph, and
/// this is a stall.
pub const DEFAULT_ONESHOT_TIMEOUT: Duration = Duration::from_secs(90);

impl Default for Unit {
    fn default() -> Self {
        Unit {
            name: String::new(),
            kind: Kind::Oneshot,
            argv: Vec::new(),
            after: Vec::new(),
            requires: Vec::new(),
            restart: Restart::Never,
            tty: None,
            log: None,
            console: false,
            timeout: None,
            ready: Vec::new(),
            ready_timeout: DEFAULT_READY_TIMEOUT,
            stop_timeout: DEFAULT_STOP_TIMEOUT,
            cgroup: Cgroup::Service,
            limits: Limits::default(),
        }
    }
}

impl Unit {
    /// A unit that provides a console is never skippable (DESIGN.md I5).
    pub fn is_console(&self) -> bool {
        self.tty.is_some()
    }
}

/// Split a command into argv. Whitespace separates; single quotes, double
/// quotes and backslash group.
///
/// This mirrors td-init's `split_argv_marked` minus the per-word "a shell
/// would have left this alone" marking, which exists only for `init
/// --dry-run`'s divergence note and has no meaning here. It is a COPY, not a
/// reuse: the build model gives each standalone target crate its own
/// 1-package lock, so there is no shared library to put it in. Drift between
/// the two is a real hazard and is covered by a test that runs the same cases
/// through both spellings.
pub fn split_argv(text: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if started {
                    out.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(q) => word.push(q),
                        None => return Err("unterminated single quote".into()),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        // POSIX: inside double quotes a backslash escapes only
                        // these four; before anything else it stays literal.
                        Some('\\') => match chars.next() {
                            Some(e) if matches!(e, '"' | '\\' | '$' | '`') => word.push(e),
                            Some(e) => {
                                word.push('\\');
                                word.push(e);
                            }
                            None => return Err("trailing backslash".into()),
                        },
                        Some(q) => word.push(q),
                        None => return Err("unterminated double quote".into()),
                    }
                }
            }
            '\\' => {
                started = true;
                match chars.next() {
                    Some(e) => word.push(e),
                    None => return Err("trailing backslash".into()),
                }
            }
            other => {
                started = true;
                word.push(other);
            }
        }
    }
    if started {
        out.push(word);
    }
    Ok(out)
}

/// An hour. Every duration this table carries is a supervision deadline, and a
/// deadline longer than this is a mistake — but more concretely, an unbounded
/// one reaches `Instant + Duration`, which PANICS on overflow, and `panic=abort`
/// makes that the end of the supervisor.
const MAX_DURATION_SECS: u64 = 3600;

fn parse_duration(value: &str) -> Result<Duration, String> {
    let seconds: u64 = value
        .parse()
        .map_err(|_| format!("'{value}' is not a whole number of seconds"))?;
    if seconds > MAX_DURATION_SECS {
        return Err(format!(
            "{seconds}s exceeds the {MAX_DURATION_SECS}s ceiling for a supervision deadline"
        ));
    }
    Ok(Duration::from_secs(seconds))
}

fn parse_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse the table. Returns every unit it understood and every complaint it
/// has; the two are independent, and a caller that wants to be strict checks
/// the complaints rather than the unit count.
pub fn parse(text: &str) -> (Vec<Unit>, Vec<String>) {
    let mut units: Vec<Unit> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let mut current: Option<Unit> = None;
    let mut stanza = Stanza::default();
    // Set when a [section] header was REJECTED. Its key lines are then swallowed
    // rather than each reported as "before any [unit]": one bad header used to
    // produce a complaint per line after it, burying the one that mattered.
    let mut orphaned = false;

    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            // Close the previous stanza FIRST, whatever this line turns out to
            // be. A malformed header used to leave `current` open, so every
            // following key silently rewrote the unit above it — the complaint
            // was logged and the wrong unit ran anyway.
            if let Some(unit) = current.take() {
                finish(unit, stanza, &mut units, &mut problems);
            }
            stanza = Stanza::default();
            orphaned = true;
            let Some(name) = rest.strip_suffix(']') else {
                problems.push(format!("line {number}: unterminated [section]"));
                continue;
            };
            let name = name.trim();
            if name.is_empty() {
                problems.push(format!("line {number}: empty unit name"));
                continue;
            }
            if !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                problems.push(format!(
                    "line {number}: unit name '{name}' must be [A-Za-z0-9_-]"
                ));
                continue;
            }
            // `.` is not in that class, and dropping it is what makes a unit
            // name safe as a cgroup directory. Every cgroup interface file is
            // spelled `<controller>.<attribute>` — `cgroup.procs`, `memory.max`
            // — and a child directory shares that namespace, so a unit named
            // for one collides with a file the kernel put there first and the
            // leaf silently cannot be made. `.` and `..` are the same rule
            // rather than a second one. A blocklist of controller prefixes
            // would rot as the kernel adds controllers; refusing the separator
            // cannot.
            // The character class above is what refuses `.`, `..` and every
            // `<controller>.<attribute>`; by here only the length can still
            // fire. `is_safe_component` is kept as the check that travels with
            // the path, so narrowing the class later cannot silently un-refuse
            // a traversal.
            if !is_safe_component(name) || name.len() > MAX_LEAF_NAME {
                problems.push(format!(
                    "line {number}: unit name '{name}' cannot name a cgroup \
                     directory, and a unit's leaf is named after it"
                ));
                continue;
            }
            // A leading `-` is refused HERE rather than left to the CLI, which
            // reads any argument starting with `-` as a flag. Such a unit would
            // run and then be impossible to name: no `status`, `start`, `stop`
            // or `restart` could ever address it.
            if name.starts_with('-') {
                problems.push(format!(
                    "line {number}: unit name '{name}' may not begin with '-'; the \
                     control client would read it as an option and the unit could \
                     never be named"
                ));
                continue;
            }
            if units.iter().any(|u| u.name == name) {
                problems.push(format!("line {number}: duplicate unit '{name}'"));
                continue;
            }
            orphaned = false;
            current = Some(Unit {
                name: name.to_string(),
                ..Unit::default()
            });
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            problems.push(format!("line {number}: expected key=value or [unit]"));
            // Fails the stanza, exactly as a rejected key does. A line the
            // parser cannot read is a line whose INTENT is unknown, and
            // admitting the unit without it silently drops that intent —
            // `requires firewall` (no `=`) would start the service with no
            // strict dependency at all, complaint logged and ignored.
            if current.is_some() {
                stanza.had_error = true;
            }
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        let Some(unit) = current.as_mut() else {
            if !orphaned {
                problems.push(format!("line {number}: '{key}' before any [unit]"));
            }
            continue;
        };
        if let Err(e) = apply(unit, key, value, &mut stanza) {
            problems.push(format!("line {number}: {e}"));
            // A rejected key fails the WHOLE stanza rather than leaving the unit
            // admitted with that key's default. The default is never harmless: a
            // refused `type=` ran the unit as the wrong kind, and a refused
            // `timeout=` leaves a oneshot able to hang forever.
            stanza.had_error = true;
        }
    }
    if let Some(unit) = current.take() {
        finish(unit, stanza, &mut units, &mut problems);
    }
    // Two units sharing one `log=` is the exact race §7 forbids for restarts,
    // reached across units instead: two sinks, two writer threads, two size
    // counters and two rotators on one file — both rename, and one reopens a
    // file the other has already moved. Refused where duplicate unit names
    // already are, because this needs the whole table to see.
    let mut seen: Vec<(&str, &str)> = Vec::new();
    let mut clashes: Vec<String> = Vec::new();
    for unit in &units {
        let Some(path) = unit.log.as_deref() else {
            continue;
        };
        let prior = seen
            .iter()
            .position(|(p, _)| *p == path)
            .and_then(|i| seen.get(i))
            .map(|(_, first)| *first);
        match prior {
            Some(first) => clashes.push(format!(
                "{}: log={path} is already used by '{first}'; two writers on one \
                 file rotate it out from under each other",
                unit.name
            )),
            None => seen.push((path, &unit.name)),
        }
    }
    if !clashes.is_empty() {
        let named: Vec<&str> = clashes
            .iter()
            .filter_map(|c| c.split(':').next())
            .collect();
        units.retain(|u| !named.iter().any(|n| *n == u.name));
        problems.extend(clashes);
    }

    (units, problems)
}

/// What a stanza's key lines established, carried to the checks that can only
/// run once it closes.
#[derive(Debug, Clone, Copy, Default)]
struct Stanza {
    saw_type: bool,
    saw_restart: bool,
    had_error: bool,
}

fn apply(unit: &mut Unit, key: &str, value: &str, stanza: &mut Stanza) -> Result<(), String> {
    match key {
        "type" => {
            // Set only on success. Marking it seen first would admit a unit
            // whose `type=` was REJECTED, silently running it as the default
            // kind — a daemon started once and never restarted, or worse.
            unit.kind = match value {
                "oneshot" => Kind::Oneshot,
                "daemon" => Kind::Daemon,
                other => return Err(format!("unknown type '{other}' (oneshot, daemon)")),
            };
            stanza.saw_type = true;
        }
        "exec" => unit.argv = split_argv(value)?,
        "ready" => unit.ready = split_argv(value)?,
        "after" => unit.after = parse_list(value),
        "requires" => unit.requires = parse_list(value),
        "restart" => {
            unit.restart = match value {
                "always" => Restart::Always,
                "on-failure" => Restart::OnFailure,
                "never" => Restart::Never,
                other => {
                    return Err(format!(
                        "unknown restart '{other}' (always, on-failure, never)"
                    ))
                }
            };
            stanza.saw_restart = true;
        }
        "tty" => {
            if value.is_empty() {
                // `/dev/` + "" is a directory; opening it fails at spawn time,
                // where the only symptom is a console that never appears.
                return Err("tty= needs a terminal".into());
            }
            unit.tty = Some(value.to_string());
        }
        "log" => {
            // An absolute path, because the value names a file the supervisor
            // opens: a relative one would resolve against whatever directory
            // PID 1 happened to leave td-svc in.
            if !value.starts_with('/') {
                return Err("log= needs an absolute path".into());
            }
            unit.log = Some(value.to_string());
        }
        "cgroup" => {
            unit.cgroup = match value {
                "service" => Cgroup::Service,
                "session" => Cgroup::Session,
                other => {
                    return Err(format!("unknown cgroup '{other}' (service, session)"));
                }
            };
        }
        "memory-max" => unit.limits.memory_max = Some(parse_bytes(value)?),
        "pids-max" => unit.limits.pids_max = Some(parse_count(value)?),
        "cpu-weight" => unit.limits.cpu_weight = Some(parse_weight(value)?),
        "console" => unit.console = parse_bool(value)?,
        "timeout" => unit.timeout = Some(parse_duration(value)?),
        "ready-timeout" => unit.ready_timeout = parse_duration(value)?,
        "stop-timeout" => unit.stop_timeout = parse_duration(value)?,
        other => return Err(format!("unknown key '{other}'")),
    }
    Ok(())
}

/// A byte count with an optional `K`, `M` or `G` suffix, all binary.
///
/// No `max`: leaving the key out is how a unit says unbounded, and a second
/// spelling for it would be a value that reads as a limit and is not one. No
/// bare `0` either — `memory.max=0` kills the service on its first page, which
/// is never what a table meant to say.
fn parse_bytes(value: &str) -> Result<u64, String> {
    // `strip_suffix` rather than `&value[..value.len() - 1]`: the slice is
    // provably in bounds and on a char boundary here, but it is still the
    // panicking construct AGENTS.md refuses, and this says the same thing.
    let (digits, scale) = match value.strip_suffix('K') {
        Some(digits) => (digits, 1024u64),
        None => match value.strip_suffix('M') {
            Some(digits) => (digits, 1024 * 1024),
            None => match value.strip_suffix('G') {
                Some(digits) => (digits, 1024 * 1024 * 1024),
                None => (value, 1),
            },
        },
    };
    let number: u64 = digits
        .parse()
        .map_err(|_| format!("expected a byte count like 64M, got '{value}'"))?;
    if number == 0 {
        return Err(format!(
            "'{value}' is zero; omit the key for unbounded rather than writing a \
             limit no process can start under"
        ));
    }
    number
        .checked_mul(scale)
        .ok_or_else(|| format!("'{value}' overflows a byte count"))
}

/// A positive count. Zero is refused for the same reason a zero byte limit is.
fn parse_count(value: &str) -> Result<u64, String> {
    let number: u64 = value
        .parse()
        .map_err(|_| format!("expected a count, got '{value}'"))?;
    if number == 0 {
        return Err(format!(
            "'{value}' is zero; a unit that may hold no process cannot start"
        ));
    }
    Ok(number)
}

/// `cpu.weight` takes 1..=10000 and the kernel refuses anything else, so this
/// refuses it here, where the diagnostic can name the unit and the table line.
fn parse_weight(value: &str) -> Result<u32, String> {
    let number: u32 = value
        .parse()
        .map_err(|_| format!("expected a cpu weight, got '{value}'"))?;
    if !(1..=10_000).contains(&number) {
        return Err(format!("cpu weight '{value}' is outside 1..=10000"));
    }
    Ok(number)
}

/// `yes`/`no`, and nothing else.
///
/// No `true`/`1`/`on` synonyms: every spelling admitted is a spelling the next
/// reader has to know, and a rejected value says what it wanted.
fn parse_bool(value: &str) -> Result<bool, String> {
    match value {
        "yes" => Ok(true),
        "no" => Ok(false),
        other => Err(format!("expected yes or no, got '{other}'")),
    }
}

/// Per-unit checks that need the whole stanza, run as it closes.
fn finish(unit: Unit, stanza: Stanza, units: &mut Vec<Unit>, problems: &mut Vec<String>) {
    let name = unit.name.clone();
    let mut ok = !stanza.had_error;
    if !stanza.saw_type {
        problems.push(format!("{name}: no type= (oneshot or daemon)"));
        ok = false;
    }
    if unit.argv.is_empty() {
        problems.push(format!("{name}: no exec="));
        ok = false;
    }
    // DESIGN.md I5. `requires` is what makes a unit skippable, and a console
    // that can be skipped is a machine that cannot be repaired from itself.
    if unit.is_console() && !unit.requires.is_empty() {
        problems.push(format!(
            "{name}: a console unit (tty=) may not declare requires= — the console is never skippable"
        ));
        ok = false;
    }
    // DESIGN.md §3: nothing is accepted-and-ignored. A `cgroup=session` unit's
    // process is moved into the session leaf by td-login before it execs, so a
    // limit written into a leaf td-svc made would bound nothing at all. The
    // pair is refused rather than the limit silently dropped.
    if unit.cgroup == Cgroup::Session && !unit.limits.is_empty() {
        problems.push(format!(
            "{name}: cgroup=session and a limit are mutually exclusive — this unit's \
             processes are accounted in the session leaf, which this unit does not own"
        ));
        ok = false;
    }
    // DESIGN.md §7. A captured stream is a PIPE, and a pipe is not a terminal:
    // the shell behind a getty would lose job control, and the greeter would
    // come up subtly broken rather than obviously so.
    if unit.is_console() && unit.log.is_some() {
        problems.push(format!(
            "{name}: tty= and log= are mutually exclusive — a captured stream is a pipe, \
             and job control needs a terminal"
        ));
        ok = false;
    }
    // Without capture there is no line to copy, and a service that inherits
    // td-svc's stderr already reaches the console. Accepting this would be the
    // accepted-and-ignored key the table forbids.
    if unit.console && unit.log.is_none() {
        problems.push(format!("{name}: console= needs log=; there is nothing to copy"));
        ok = false;
    }
    // A unit cannot require itself: that is a cycle stated in one line, and
    // the graph would report it as one without saying which key caused it.
    if unit.after.contains(&name) || unit.requires.contains(&name) {
        problems.push(format!("{name}: depends on itself"));
        ok = false;
    }
    if unit.kind == Kind::Oneshot && !unit.ready.is_empty() {
        problems.push(format!(
            "{name}: ready= applies to daemons; a oneshot is ready when it exits 0"
        ));
        ok = false;
    }
    // A oneshot is never restarted — it runs to completion and its exit status
    // is the answer. `restart=always` on one was accepted and silently did
    // nothing, which reads in the table as a guarantee the supervisor does not
    // make.
    if unit.kind == Kind::Oneshot && stanza.saw_restart {
        problems.push(format!(
            "{name}: restart= applies to daemons; a oneshot runs once"
        ));
        ok = false;
    }
    if unit.kind == Kind::Daemon && unit.timeout.is_some() {
        problems.push(format!(
            "{name}: timeout= applies to oneshots; a daemon uses ready-timeout="
        ));
        ok = false;
    }
    let mut unit = unit;
    apply_defaults(&mut unit);
    if ok {
        units.push(unit);
    }
}

/// Fill in what the table left unsaid. Separate from validation because these
/// are not diagnostics — a table without them is correct, just under-specified.
fn apply_defaults(unit: &mut Unit) {
    if unit.kind == Kind::Oneshot && unit.timeout.is_none() {
        unit.timeout = Some(DEFAULT_ONESHOT_TIMEOUT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_table_parses() {
        let (units, problems) = parse("[a]\ntype=oneshot\nexec=/bin/true\n");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, "a");
        assert_eq!(units[0].kind, Kind::Oneshot);
        assert_eq!(units[0].argv, ["/bin/true"]);
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let (units, problems) = parse("# a comment\n\n[a]\n# another\ntype=daemon\nexec=/bin/x\n\n");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(units.len(), 1);
    }

    #[test]
    fn every_key_round_trips() {
        let (units, problems) = parse(
            "[sshd]\n\
             type=daemon\n\
             exec=/bin/sshd -D -e -f /etc/ssh/sshd_config\n\
             after=netup, td-firstboot\n\
             restart=always\n\
             ready=/bin/td-netd reach 127.0.0.1 22\n\
             ready-timeout=45\n\
             stop-timeout=5\n",
        );
        assert!(problems.is_empty(), "{problems:?}");
        let u = &units[0];
        assert_eq!(u.kind, Kind::Daemon);
        assert_eq!(
            u.argv,
            ["/bin/sshd", "-D", "-e", "-f", "/etc/ssh/sshd_config"]
        );
        assert_eq!(u.after, ["netup", "td-firstboot"]);
        assert_eq!(u.restart, Restart::Always);
        assert_eq!(u.ready, ["/bin/td-netd", "reach", "127.0.0.1", "22"]);
        assert_eq!(u.ready_timeout, Duration::from_secs(45));
        assert_eq!(u.stop_timeout, Duration::from_secs(5));
    }

    /// A bad stanza must not take the good ones with it — that is the whole
    /// reason parsing collects rather than aborts.
    #[test]
    fn a_rejected_unit_does_not_stop_the_others() {
        let (units, problems) = parse(
            "[good]\ntype=oneshot\nexec=/bin/true\n\
             [bad]\ntype=nonsense\nexec=/bin/true\n\
             [alsogood]\ntype=daemon\nexec=/bin/x\n",
        );
        let names: Vec<&str> = units.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, ["good", "alsogood"]);
        assert!(problems.iter().any(|p| p.contains("unknown type")));
    }

    #[test]
    fn the_console_may_not_be_made_skippable() {
        let (units, problems) = parse(
            "[greeter]\ntype=daemon\nexec=/etc/tty-session\ntty=ttyS0\nrequires=netup\n",
        );
        assert!(units.is_empty());
        assert!(
            problems.iter().any(|p| p.contains("never skippable")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_console_unit_may_still_declare_ordering() {
        let (units, problems) =
            parse("[greeter]\ntype=daemon\nexec=/etc/tty-session\ntty=ttyS0\nafter=netup\n");
        assert!(problems.is_empty(), "{problems:?}");
        assert!(units[0].is_console());
        assert_eq!(units[0].after, ["netup"]);
    }

    /// `log=` names where output is captured, and `console=` copies it.
    #[test]
    fn a_unit_can_declare_where_its_output_goes() {
        let (units, problems) = parse(
            "[g]\ntype=daemon\nexec=/x\nlog=/var/log/svc/g.log\nconsole=yes\n",
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            units.first().and_then(|u| u.log.as_deref()),
            Some("/var/log/svc/g.log")
        );
        assert_eq!(units.first().map(|u| u.console), Some(true));
    }

    /// A relative `log=` would resolve against whatever directory PID 1 left
    /// td-svc in, which is not a place a table can name.
    #[test]
    fn a_relative_log_path_is_refused() {
        let (units, problems) = parse("[g]\ntype=daemon\nexec=/x\nlog=var/g.log\n");
        assert!(units.is_empty(), "a relative log path was admitted");
        assert!(
            problems.iter().any(|p| p.contains("absolute")),
            "{problems:?}"
        );
    }

    /// The limit keys, and the units they produce.
    #[test]
    fn a_unit_declares_its_own_bounds() {
        let (units, problems) = parse(
            "[g]\ntype=daemon\nexec=/x\nmemory-max=64M\npids-max=32\ncpu-weight=200\n",
        );
        assert!(problems.is_empty(), "{problems:?}");
        let g = units.first().expect("one unit");
        assert_eq!(g.limits.memory_max, Some(64 * 1024 * 1024));
        assert_eq!(g.limits.pids_max, Some(32));
        assert_eq!(g.limits.cpu_weight, Some(200));
        assert_eq!(g.cgroup, Cgroup::Service);
        assert_eq!(g.cgroup_leaf_name(), Some("g"));
    }

    /// A unit with nothing to say about its bounds says nothing, and gets the
    /// kernel's own default rather than a number this table invented.
    #[test]
    fn an_undeclared_limit_is_unbounded_not_defaulted() {
        let (units, problems) = parse("[g]\ntype=daemon\nexec=/x\n");
        assert!(problems.is_empty(), "{problems:?}");
        let g = units.first().expect("one unit");
        assert!(g.limits.is_empty());
        assert_eq!(g.limits.memory_max, None);
    }

    /// Sizes take a binary suffix, and refuse what cannot be honoured. Zero is
    /// the one that matters: `memory.max=0` kills the service on its first page.
    #[test]
    fn a_size_is_binary_and_never_zero() {
        for (text, want) in [("512", 512u64), ("1K", 1024), ("2M", 2 << 20), ("1G", 1 << 30)] {
            assert_eq!(parse_bytes(text), Ok(want), "{text}");
        }
        for bad in ["0", "0M", "", "M", "-1", "64MB", "1.5M", "99999999999999999999G"] {
            assert!(parse_bytes(bad).is_err(), "{bad} was accepted");
        }
        assert!(parse_count("0").is_err(), "a unit may not hold zero processes");
        assert_eq!(parse_count("32"), Ok(32));
        for bad in ["0", "10001", "-1", "x"] {
            assert!(parse_weight(bad).is_err(), "cpu weight {bad} was accepted");
        }
        assert_eq!(parse_weight("1"), Ok(1));
        assert_eq!(parse_weight("10000"), Ok(10_000));
    }

    /// `cgroup=` takes the two placements and no third spelling.
    #[test]
    fn cgroup_takes_only_service_or_session() {
        let (units, problems) = parse("[g]\ntype=daemon\nexec=/x\ncgroup=session\n");
        assert!(problems.is_empty(), "{problems:?}");
        let g = units.first().expect("one unit");
        assert_eq!(g.cgroup, Cgroup::Session);
        assert_eq!(
            g.cgroup_leaf_name(),
            None,
            "a session unit owns no leaf, so it names none"
        );
        for value in ["yes", "own", "system", ""] {
            let (units, problems) =
                parse(&format!("[g]\ntype=daemon\nexec=/x\ncgroup={value}\n"));
            assert!(units.is_empty(), "cgroup={value} was admitted");
            assert!(problems.iter().any(|p| p.contains("unknown cgroup")), "{problems:?}");
        }
    }

    /// A limit on a unit whose processes are accounted elsewhere would bound
    /// nothing. Refused as a pair, the way `tty=` with `log=` is.
    #[test]
    fn a_session_unit_may_not_declare_a_limit_it_cannot_hold() {
        let (units, problems) = parse(
            "[g]\ntype=daemon\nexec=/x\ncgroup=session\nmemory-max=64M\n",
        );
        assert!(units.is_empty(), "a limit was admitted on a session unit");
        assert!(
            problems.iter().any(|p| p.contains("mutually exclusive")),
            "{problems:?}"
        );
        // The same unit without the limit is fine: the placement is not the
        // problem, the unenforceable promise is.
        let (units, problems) = parse("[g]\ntype=daemon\nexec=/x\ncgroup=session\n");
        assert_eq!(units.len(), 1, "{problems:?}");
    }

    /// A unit name becomes a directory under `/sys/fs/cgroup`, and a cgroup
    /// directory shares its namespace with the kernel's own interface files.
    /// Every one of those is `<controller>.<attribute>`, so refusing the
    /// separator refuses the whole class at once — `.` and `..` included, and
    /// controllers the kernel has not added yet.
    #[test]
    fn a_unit_may_not_be_named_for_a_cgroup_file() {
        // `.` and `..` traverse; the rest are real interface files that exist
        // in every cgroup directory before td-svc creates anything.
        for name in [".", "..", "cgroup.procs", "memory.max", "pids.max", "cpu.weight"] {
            let (units, problems) = parse(&format!("[{name}]\ntype=daemon\nexec=/x\n"));
            assert!(units.is_empty(), "a unit named '{name}' was admitted");
            assert!(
                problems.iter().any(|p| p.contains(name)),
                "{name}: {problems:?}"
            );
        }
        // Length is the other way a legal name stops being a legal directory.
        let long = "a".repeat(MAX_LEAF_NAME + 1);
        let (units, problems) = parse(&format!("[{long}]\ntype=daemon\nexec=/x\n"));
        assert!(units.is_empty(), "a {}-byte name was admitted", long.len());
        assert!(
            problems.iter().any(|p| p.contains("cannot name a cgroup")),
            "{problems:?}"
        );
        // The longest name that still fits is not refused.
        let longest = "a".repeat(MAX_LEAF_NAME);
        let (units, problems) = parse(&format!("[{longest}]\ntype=daemon\nexec=/x\n"));
        assert_eq!(units.len(), 1, "{problems:?}");
        assert!(!is_safe_component("a/b"));
        assert!(!is_safe_component(""));
    }

    /// `console=` takes yes or no, and says so when it does not get one.
    #[test]
    fn console_takes_only_yes_or_no() {
        for value in ["true", "1", "on", ""] {
            let (units, problems) = parse(&format!(
                "[g]\ntype=daemon\nexec=/x\nlog=/var/g.log\nconsole={value}\n"
            ));
            assert!(units.is_empty(), "console={value} was admitted");
            assert!(
                problems.iter().any(|p| p.contains("expected yes or no")),
                "console={value}: {problems:?}"
            );
        }
    }

    /// Two units may not share one `log=`.
    ///
    /// The same race the module refuses for restarts, reached across units
    /// instead: two sinks, two writer threads, two size counters and two
    /// rotators on one file.
    #[test]
    fn two_units_may_not_write_to_the_same_log() {
        let (units, problems) = parse(
            "[a]\ntype=daemon\nexec=/x\nlog=/var/log/one.log\n\n\
             [b]\ntype=daemon\nexec=/y\nlog=/var/log/one.log\n",
        );
        assert!(
            !units.iter().any(|u| u.name == "b"),
            "the second claimant on one log file was admitted"
        );
        assert!(
            problems.iter().any(|p| p.contains("already used by")),
            "{problems:?}"
        );

        // Different paths are of course fine.
        let (units, problems) = parse(
            "[a]\ntype=daemon\nexec=/x\nlog=/var/log/one.log\n\n\
             [b]\ntype=daemon\nexec=/y\nlog=/var/log/two.log\n",
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(units.len(), 2);
    }

    /// DESIGN.md §7: a captured stream is a pipe, and job control needs a
    /// terminal. Accepting both would produce a greeter that comes up subtly
    /// broken instead of obviously so.
    #[test]
    fn a_console_unit_may_not_also_be_captured() {
        let (units, problems) =
            parse("[g]\ntype=daemon\nexec=/x\ntty=ttyS0\nlog=/var/log/g.log\n");
        assert!(units.is_empty(), "tty= and log= were admitted together");
        assert!(
            problems.iter().any(|p| p.contains("mutually exclusive")),
            "{problems:?}"
        );
    }

    /// `console=yes` with nothing captured is the accepted-and-ignored key the
    /// table forbids: there is no line to copy, and inheriting td-svc's stderr
    /// already reaches the console.
    #[test]
    fn copying_to_the_console_needs_something_to_copy() {
        let (units, problems) = parse("[g]\ntype=daemon\nexec=/x\nconsole=yes\n");
        assert!(units.is_empty(), "console= without log= was admitted");
        assert!(
            problems.iter().any(|p| p.contains("needs log=")),
            "{problems:?}"
        );
    }

    /// `/dev/` + "" is a directory. Left to spawn time the only symptom is a
    /// console that never appears.
    #[test]
    fn an_empty_terminal_name_is_refused() {
        let (units, problems) = parse("[g]\ntype=daemon\nexec=/x\ntty=\n");
        assert!(units.is_empty());
        assert!(problems.iter().any(|p| p.contains("needs a terminal")), "{problems:?}");
    }

    /// A line the parser cannot read has an UNKNOWN intent, so it fails its
    /// stanza like a rejected key. `requires firewall` (no `=`) used to be
    /// logged and then ignored, starting the service with no strict dependency.
    #[test]
    fn a_line_that_is_not_key_value_fails_its_stanza() {
        let (units, problems) =
            parse("[svc]\ntype=daemon\nexec=/x\nrequires firewall\n");
        assert!(units.is_empty(), "a unit whose intent was not parsed must not run");
        assert!(problems.iter().any(|p| p.contains("expected key=value")));
    }

    #[test]
    fn missing_required_keys_are_reported_by_name() {
        let (units, problems) = parse("[a]\nexec=/bin/true\n");
        assert!(units.is_empty());
        assert!(problems.iter().any(|p| p.contains("no type=")));

        let (units, problems) = parse("[a]\ntype=oneshot\n");
        assert!(units.is_empty());
        assert!(problems.iter().any(|p| p.contains("no exec=")));
    }

    #[test]
    fn structural_errors_name_their_line() {
        let (_, problems) = parse("type=oneshot\n");
        assert!(problems[0].starts_with("line 1:"));
        assert!(problems[0].contains("before any [unit]"));

        let (_, problems) = parse("[a\ntype=oneshot\n");
        assert!(problems[0].contains("unterminated"));

        let (_, problems) = parse("[a]\ntype=oneshot\nexec=/x\nnonsense\n");
        assert!(problems.iter().any(|p| p.contains("line 4")));
    }

    #[test]
    fn duplicate_units_are_rejected_rather_than_silently_merged() {
        let (units, problems) =
            parse("[a]\ntype=oneshot\nexec=/x\n[a]\ntype=oneshot\nexec=/y\n");
        assert_eq!(units.len(), 1);
        assert!(problems.iter().any(|p| p.contains("duplicate")));
    }

    /// A malformed header must not leave the previous stanza open — the keys
    /// after it would silently rewrite the unit above, which then RUNS despite
    /// the logged complaint.
    #[test]
    fn a_malformed_section_header_closes_the_stanza_above_it() {
        let (units, problems) = parse(
            "[good]\ntype=oneshot\nexec=/bin/true\n\
             [broken\nexec=/bin/evil\ntype=daemon\n",
        );
        assert!(problems.iter().any(|p| p.contains("unterminated")));
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, "good");
        assert_eq!(units[0].argv, ["/bin/true"], "the stanza above was rewritten");
        assert_eq!(units[0].kind, Kind::Oneshot);
    }

    /// An unbounded deadline reaches `Instant + Duration`, which panics on
    /// overflow — and `panic=abort` makes that the end of the supervisor.
    #[test]
    fn an_absurd_duration_is_refused_rather_than_carried_to_an_overflow() {
        let (units, problems) = parse(&format!(
            "[a]\ntype=oneshot\nexec=/x\ntimeout={}\n",
            u64::MAX
        ));
        assert!(units.is_empty());
        assert!(problems.iter().any(|p| p.contains("ceiling")), "{problems:?}");
        let (_, problems) = parse("[a]\ntype=oneshot\nexec=/x\ntimeout=3600\n");
        assert!(problems.is_empty(), "the ceiling itself must be accepted");
    }

    #[test]
    fn a_self_dependency_is_named_by_the_key_that_caused_it() {
        let (units, problems) = parse("[a]\ntype=oneshot\nexec=/x\nafter=a\n");
        assert!(units.is_empty());
        assert!(problems.iter().any(|p| p.contains("depends on itself")));
    }

    /// A oneshot with no `timeout=` HANGING is how the console gets stranded
    /// from a table `check` calls clean: it never settles, so everything
    /// ordered after it waits forever. Every oneshot therefore carries a
    /// deadline whether or not the table names one.
    #[test]
    fn every_oneshot_gets_a_deadline_even_when_the_table_omits_one() {
        let (units, problems) = parse("[a]\ntype=oneshot\nexec=/x\n");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(units[0].timeout, Some(DEFAULT_ONESHOT_TIMEOUT));
        // An explicit one still wins.
        let (units, _) = parse("[a]\ntype=oneshot\nexec=/x\ntimeout=5\n");
        assert_eq!(units[0].timeout, Some(Duration::from_secs(5)));
        // A daemon has no timeout= at all; `ready-timeout` bounds it instead.
        let (units, _) = parse("[a]\ntype=daemon\nexec=/x\n");
        assert_eq!(units[0].timeout, None);
    }

    #[test]
    fn readiness_and_timeout_belong_to_the_right_kind() {
        let (_, problems) = parse("[a]\ntype=oneshot\nexec=/x\nready=/bin/true\n");
        assert!(problems.iter().any(|p| p.contains("ready= applies to daemons")));
        let (_, problems) = parse("[a]\ntype=daemon\nexec=/x\ntimeout=5\n");
        assert!(problems.iter().any(|p| p.contains("timeout= applies to oneshots")));
    }

    /// `restart=` on a oneshot was accepted and then ignored — the table said
    /// the supervisor would restart it and the supervisor never would.
    #[test]
    fn restart_belongs_to_daemons_only() {
        let (units, problems) = parse("[a]\ntype=oneshot\nexec=/x\nrestart=always\n");
        assert!(units.is_empty());
        assert!(
            problems.iter().any(|p| p.contains("restart= applies to daemons")),
            "{problems:?}"
        );
        // Including the one that matches the default: it still reads as a
        // promise, and the point is that the key has no meaning here.
        let (_, problems) = parse("[a]\ntype=oneshot\nexec=/x\nrestart=never\n");
        assert!(problems.iter().any(|p| p.contains("restart= applies to daemons")));
        let (_, problems) = parse("[a]\ntype=daemon\nexec=/x\nrestart=always\n");
        assert!(problems.is_empty(), "{problems:?}");
    }

    /// One bad header used to produce a complaint for every key line beneath
    /// it, burying the one complaint that explained the whole stanza.
    #[test]
    fn a_rejected_header_does_not_cascade_a_complaint_per_key() {
        let (units, problems) = parse(
            "[a\ntype=daemon\nexec=/x\nafter=b\nrestart=always\nready=/bin/true\n",
        );
        assert!(units.is_empty());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("unterminated"));

        // A key genuinely before ANY header is still reported — that is a
        // different mistake and the only complaint that can explain it.
        let (_, problems) = parse("type=oneshot\n[a]\ntype=oneshot\nexec=/x\n");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("before any [unit]"));
    }

    #[test]
    fn argv_splitting_handles_quotes_and_escapes() {
        assert_eq!(split_argv("a b c").unwrap(), ["a", "b", "c"]);
        assert_eq!(split_argv("a  \t b").unwrap(), ["a", "b"]);
        assert_eq!(split_argv("'one word'").unwrap(), ["one word"]);
        assert_eq!(split_argv("\"one word\"").unwrap(), ["one word"]);
        assert_eq!(split_argv(r"a\ b").unwrap(), ["a b"]);
        assert_eq!(split_argv(r#""a\"b""#).unwrap(), [r#"a"b"#]);
        // Only the POSIX four are escapes inside double quotes.
        assert_eq!(split_argv(r#""a\nb""#).unwrap(), [r"a\nb"]);
        assert_eq!(split_argv("").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn unterminated_quoting_is_an_error_not_a_truncation() {
        assert!(split_argv("'unclosed").is_err());
        assert!(split_argv("\"unclosed").is_err());
        assert!(split_argv("trailing\\").is_err());
        let (_, problems) = parse("[a]\ntype=oneshot\nexec=/bin/x 'oops\n");
        assert!(problems.iter().any(|p| p.contains("unterminated single quote")));
    }

    /// The splitter is a COPY of td-init's, so these are the cases that would
    /// diverge first if one side were edited alone.
    #[test]
    fn the_splitter_matches_td_inits_documented_behaviour() {
        // Quoted whitespace groups; adjacent quoting concatenates.
        assert_eq!(split_argv("sh -c 'a; b'").unwrap(), ["sh", "-c", "a; b"]);
        assert_eq!(split_argv("a'b'c").unwrap(), ["abc"]);
        // A metacharacter is passed through literally — td execs directly, it
        // does not run a shell.
        assert_eq!(split_argv("cmd>/dev/log").unwrap(), ["cmd>/dev/log"]);
        assert_eq!(split_argv("$HOME").unwrap(), ["$HOME"]);
    }

    /// A unit the control client could never name is refused at parse time.
    ///
    /// `route()` reads any argument beginning with `-` as an option, so a
    /// `[-greeter]` would start on boot and then be unreachable by every verb
    /// the socket offers. The parser is where that is knowable.
    #[test]
    fn a_unit_name_may_not_begin_with_a_dash() {
        let (units, problems) = parse("[-greeter]\ntype=daemon\nexec=/bin/sh\n");
        assert!(
            units.is_empty(),
            "a unit no verb could ever address was admitted"
        );
        assert!(
            problems.iter().any(|p| p.contains("may not begin with")),
            "{problems:?}"
        );

        // A dash elsewhere is still fine — the rule is about the FIRST
        // character, not the character.
        let (units, problems) = parse("[tty-greeter]\ntype=daemon\nexec=/bin/sh\n");
        assert_eq!(units.len(), 1, "{problems:?}");
    }
}
