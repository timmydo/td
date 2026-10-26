//! Flathub applications fetched on demand (APPLICATIONS.md §W.9): the
//! reviewed declarations, the per-account cache layout, and the pure halves
//! of admission.
//!
//! `td-app` (td-net) performs the I/O. Everything here is std-only, does
//! none and uses no other module, so td-jail can `#[path]`-include it to
//! read the same `selection` and pointers td-app writes.

/// One declared Flathub application: its app ref and the one runtime ref it
/// accepts, both fetched. The bytes are not reviewed; these names, Flathub's
/// signing key and the admission rules are.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlathubSource {
    /// The td application name: its registry row and launcher name.
    pub name: &'static str,
    /// The app ref.
    pub application: &'static str,
    /// The one runtime ref the application accepts.
    pub runtime: &'static str,
    /// The application's entry inside its namespace.
    pub entry: &'static str,
    /// The runtime key of the application's dynamic-library policy.
    pub policy_runtime: &'static str,
}

pub const REPOSITORY: &str = "https://dl.flathub.org/repo";

const PLATFORM_25_08: &str = "runtime/org.freedesktop.Platform/x86_64/25.08";

pub const FLATHUB_SOURCES: &[FlathubSource] = &[FlathubSource {
    name: "firefox",
    application: "app/org.mozilla.firefox/x86_64/stable",
    runtime: PLATFORM_25_08,
    entry: "/app/bin/firefox",
    policy_runtime: "freedesktop-platform-25-08",
}];

pub fn flathub_source(name: &str) -> Option<FlathubSource> {
    FLATHUB_SOURCES
        .iter()
        .find(|source| source.name == name)
        .copied()
}

/// Where a spec's trees come from. A `/td/store` runtime path takes both
/// from the store and carries no `source=`; any other `runtime=` must be the
/// declaration's runtime ref beside `source=` its app ref, and both trees
/// come from the account's cache. The spec compiler and td-jail both hold
/// specs to this.
pub fn spec_source(
    name: &str,
    source: Option<&str>,
    runtime: &str,
) -> Result<Option<FlathubSource>, String> {
    if runtime.starts_with('/') {
        return match source {
            None => Ok(None),
            Some(_) => Err("an application source needs a Flathub runtime ref".into()),
        };
    }
    let declared =
        flathub_source(name).ok_or_else(|| format!("{name} declares no Flathub runtime ref"))?;
    if runtime != declared.runtime || source != Some(declared.application) {
        return Err(format!(
            "{name}'s spec does not name its declared Flathub refs"
        ));
    }
    Ok(Some(declared))
}

/// The cache, relative to the application account's home.
pub const CACHE_DIR: &str = ".td/flathub";
/// A feed endpoint named on the kernel command line, for a guest no VM
/// bridge serves (the QEMU oracles): the deployment init writes the token's
/// value here, root-owned, and td-app reads it after the bridge's.
pub const BOOT_FEED: &str = "/run/td-feed";
pub const BOOT_FEED_TOKEN: &str = "td.feed=";
pub const LOCK: &str = "lock";
pub const OBJECTS: &str = "objects";
pub const DEPLOY: &str = "deploy";
pub const GENERATIONS: &str = "generations";
pub const SELECTION: &str = "selection";
pub const CURRENT: &str = "current";
pub const PREVIOUS: &str = "previous";
pub const ADMITTED: &str = "admitted";
pub const CHECKED: &str = "checked";
/// The last check or revalidation that did not complete, until one does.
pub const REFUSED: &str = "refused";
pub const WORK: &str = ".work";
/// Below [`WORK`]: what the writer holding the lock is doing.
pub const PROGRESS: &str = "progress";
/// Every name td-app keeps in the cache root; anything else is debris.
pub const CACHE_ENTRIES: &[&str] = &[
    LOCK,
    OBJECTS,
    DEPLOY,
    GENERATIONS,
    CURRENT,
    PREVIOUS,
    ADMITTED,
    CHECKED,
    REFUSED,
    WORK,
];
/// A deploy's tree, below `deploy/<commit>/`.
pub const FILES: &str = "files";

pub const MAX_STATE_BYTES: usize = 4096;
const SELECTION_FORMAT: &str = "format=1";
const MAX_REF_BYTES: usize = 255;
const MAX_NAME_BYTES: usize = 64;

/// One admitted commit of one ref.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Admitted {
    pub exact_ref: String,
    pub commit: String,
    pub timestamp: u64,
}

/// What a generation runs, and what it was validated as.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Selection {
    /// The declaration it was admitted for.
    pub name: String,
    /// A digest of the rule-5 inputs no commit carries: the entry and the
    /// dynamic-library policy. td-app computes it.
    pub checks: String,
    pub application: Admitted,
    pub runtime: Admitted,
}

impl Selection {
    pub fn render(&self) -> String {
        format!(
            "{SELECTION_FORMAT}\nname={}\nchecks={}\n{}{}",
            self.name,
            self.checks,
            render_row("app", &self.application),
            render_row("runtime", &self.runtime)
        )
    }

    pub fn parse(text: &str) -> Result<Selection, String> {
        if text.len() > MAX_STATE_BYTES {
            return Err(format!("selection exceeds {MAX_STATE_BYTES} bytes"));
        }
        if !text.ends_with('\n') {
            return Err("selection does not end in a newline".into());
        }
        let mut lines = text.split_terminator('\n');
        if lines.next() != Some(SELECTION_FORMAT) {
            return Err("selection has no format=1 header".into());
        }
        let name = field(lines.next(), "name")?;
        if !valid_name(name) {
            return Err(format!("selection name {name:?} is malformed"));
        }
        let checks = field(lines.next(), "checks")?;
        if !valid_hex64(checks) {
            return Err("selection checks is not a SHA-256".into());
        }
        let application = parse_row(field(lines.next(), "app")?)?;
        require_kind(&application, "app")?;
        let runtime = parse_row(field(lines.next(), "runtime")?)?;
        require_kind(&runtime, "runtime")?;
        if let Some(line) = lines.next() {
            return Err(format!("selection line {line:?} is out of place"));
        }
        Ok(Selection {
            name: name.to_string(),
            checks: checks.to_string(),
            application,
            runtime,
        })
    }

    /// The commits this generation keeps in the cache.
    pub fn commits(&self) -> impl Iterator<Item = &str> {
        self.rows().map(|admitted| admitted.commit.as_str())
    }

    pub fn rows(&self) -> impl Iterator<Item = &Admitted> {
        [&self.application, &self.runtime].into_iter()
    }
}

fn field<'a>(line: Option<&'a str>, key: &str) -> Result<&'a str, String> {
    line.and_then(|line| line.strip_prefix(key))
        .and_then(|rest| rest.strip_prefix('='))
        .ok_or_else(|| format!("selection lacks its {key}= line"))
}

fn render_row(key: &str, admitted: &Admitted) -> String {
    format!(
        "{key}={}\t{}\t{}\n",
        admitted.exact_ref, admitted.commit, admitted.timestamp
    )
}

fn parse_row(row: &str) -> Result<Admitted, String> {
    let mut fields = row.split('\t');
    let (Some(exact_ref), Some(commit), Some(timestamp), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return Err(format!("row {row:?} is not ref, commit and timestamp"));
    };
    if !valid_ref(exact_ref) {
        return Err(format!("row {row:?} names a malformed ref"));
    }
    if !valid_hex64(commit) {
        return Err(format!("row {row:?} names a malformed commit"));
    }
    if timestamp.is_empty()
        || (timestamp.len() > 1 && timestamp.starts_with('0'))
        || !timestamp.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!("row {row:?} has a malformed timestamp"));
    }
    let timestamp = timestamp
        .parse()
        .map_err(|_| format!("row {row:?} has a timestamp out of range"))?;
    Ok(Admitted {
        exact_ref: exact_ref.to_string(),
        commit: commit.to_string(),
        timestamp,
    })
}

fn require_kind(admitted: &Admitted, kind: &str) -> Result<(), String> {
    if admitted.exact_ref.split('/').next() != Some(kind) {
        return Err(format!(
            "selection {kind} row names {:?}",
            admitted.exact_ref
        ));
    }
    Ok(())
}

/// `app/` or `runtime/` and three components of `[A-Za-z0-9._-]`, the
/// shape of every declared ref.
pub fn valid_ref(exact_ref: &str) -> bool {
    let mut parts = exact_ref.split('/');
    exact_ref.len() <= MAX_REF_BYTES
        && matches!(parts.next(), Some("app" | "runtime"))
        && parts.clone().count() == 3
        && parts.all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        })
}

pub fn valid_hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// A generation directory's name: decimal, nonzero, no leading zero.
pub fn generation_number(name: &str) -> Option<u64> {
    if name.is_empty() || name.starts_with('0') || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

/// What `current` and `previous` hold: exactly `generations/<n>`.
pub fn pointer_target(generation: u64) -> String {
    format!("{GENERATIONS}/{generation}")
}

pub fn parse_pointer(target: &str) -> Result<u64, String> {
    target
        .strip_prefix(GENERATIONS)
        .and_then(|rest| rest.strip_prefix('/'))
        .and_then(generation_number)
        .ok_or_else(|| format!("pointer {target:?} does not name a generation"))
}

/// The newest commit admitted for each ref, one row per ref, sorted.
pub fn render_admitted(rows: &[Admitted]) -> String {
    let mut rows: Vec<&Admitted> = rows.iter().collect();
    rows.sort_by(|left, right| left.exact_ref.cmp(&right.exact_ref));
    rows.iter()
        .map(|admitted| render_row("admitted", admitted))
        .collect()
}

pub fn parse_admitted(text: &str) -> Result<Vec<Admitted>, String> {
    if text.len() > MAX_STATE_BYTES {
        return Err(format!("admitted state exceeds {MAX_STATE_BYTES} bytes"));
    }
    if !text.is_empty() && !text.ends_with('\n') {
        return Err("admitted state does not end in a newline".into());
    }
    let mut rows: Vec<Admitted> = Vec::new();
    for line in text.split_terminator('\n') {
        let row = line
            .strip_prefix("admitted=")
            .ok_or_else(|| format!("admitted line {line:?} is malformed"))?;
        let admitted = parse_row(row)?;
        if rows
            .last()
            .is_some_and(|prior| prior.exact_ref >= admitted.exact_ref)
        {
            return Err("admitted state is not strictly sorted by ref".into());
        }
        rows.push(admitted);
    }
    Ok(rows)
}

/// The rule-3 floor for `exact_ref`: the newest of every row that names
/// it, so a lost or stale `admitted` never falls below what is selected.
pub fn floor<'a>(
    rows: impl IntoIterator<Item = &'a Admitted>,
    exact_ref: &str,
) -> Option<&'a Admitted> {
    rows.into_iter()
        .filter(|row| row.exact_ref == exact_ref)
        .max_by_key(|row| row.timestamp)
}

/// Rule 3: a candidate may not be older than the newest commit admitted for
/// its ref, and an equal timestamp admits only that same commit, so an old
/// signed commit cannot be replayed as an update.
pub fn admits_after(
    previous: Option<&Admitted>,
    commit: &str,
    timestamp: u64,
) -> Result<(), String> {
    let Some(previous) = previous else {
        return Ok(());
    };
    if timestamp < previous.timestamp
        || (timestamp == previous.timestamp && commit != previous.commit)
    {
        return Err(format!(
            "{} commit {commit} at {timestamp} does not supersede admitted {} at {}",
            previous.exact_ref, previous.commit, previous.timestamp
        ));
    }
    Ok(())
}

/// The `runtime=` of a Flatpak commit's `[Application]` group, which names
/// a runtime ref without its `runtime/` prefix.
pub fn application_runtime(metadata: &str) -> Result<&str, String> {
    let mut in_application = false;
    let mut found = None;
    for line in metadata.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(group) = line.strip_prefix('[') {
            in_application = group == "Application]";
            continue;
        }
        if !in_application {
            continue;
        }
        if let Some(value) = line.strip_prefix("runtime=") {
            if found.replace(value).is_some() {
                return Err("[Application] names runtime= twice".into());
            }
        }
    }
    found.ok_or_else(|| "commit metadata has no [Application] runtime=".to_string())
}

/// Rule 4: the application's metadata names exactly the accepted runtime.
pub fn require_runtime(metadata: &str, runtime_ref: &str) -> Result<(), String> {
    let named = application_runtime(metadata)?;
    let accepted = runtime_ref
        .strip_prefix("runtime/")
        .ok_or_else(|| format!("{runtime_ref:?} is not a runtime ref"))?;
    if named != accepted {
        return Err(format!(
            "application needs runtime {named:?}, and only {accepted:?} is accepted"
        ));
    }
    Ok(())
}

/// `td-app status`'s answer and the root operation's relay of it
/// (td-software/DESIGN.md §3): one bounded, canonical record.
pub const MAX_STATUS_BYTES: usize = 4096;
/// The longest refusal reason a status carries; td-app cuts a longer one.
pub const MAX_REASON_BYTES: usize = 512;
const STATUS_FORMAT: &str = "format=1";

/// What a status answers for one registry application.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Status {
    pub name: String,
    /// `None` for an application the image ships, whose bytes are the
    /// deployment's and which has no cache.
    pub cache: Option<CacheStatus>,
}

/// A Flathub application's cache, as td-app read it without its lock.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheStatus {
    pub current: Option<GenerationStatus>,
    pub previous: Option<GenerationStatus>,
    /// The last completed check.
    pub checked: Option<u64>,
    /// The last check or revalidation that did not complete, since the
    /// last one that did.
    pub refused: Option<Refusal>,
    /// What the writer holding the cache's lock is doing, if one is.
    pub progress: Option<Progress>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenerationStatus {
    pub generation: u64,
    /// Whether it still serves the declaration (`td-app ready`'s test).
    pub serves: bool,
    pub application: DeployStatus,
    pub runtime: DeployStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeployStatus {
    pub row: Admitted,
    /// The regular-file bytes of its deploy, or `None` when the deploy is
    /// incomplete or could not be measured.
    pub bytes: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Refusal {
    pub time: u64,
    /// Printable ASCII without tabs, at most [`MAX_REASON_BYTES`].
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    /// A writer holds the lock and has published no record yet.
    Busy,
    Resolve,
    Fetch,
    Materialize,
    Validate,
    Publish,
    Collect,
    Revalidate,
    Remove,
    DeleteData,
}

const STAGES: &[(Stage, &str)] = &[
    (Stage::Busy, "busy"),
    (Stage::Resolve, "resolve"),
    (Stage::Fetch, "fetch"),
    (Stage::Materialize, "materialize"),
    (Stage::Validate, "validate"),
    (Stage::Publish, "publish"),
    (Stage::Collect, "collect"),
    (Stage::Revalidate, "revalidate"),
    (Stage::Remove, "remove"),
    (Stage::DeleteData, "delete-data"),
];

impl Stage {
    pub fn word(self) -> &'static str {
        STAGES
            .iter()
            .find(|(stage, _)| *stage == self)
            .map_or("busy", |(_, word)| word)
    }

    fn parse(word: &str) -> Option<Stage> {
        STAGES
            .iter()
            .find(|(_, known)| *known == word)
            .map(|(stage, _)| *stage)
    }
}

/// Which of the declaration's refs a stage works on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Part {
    Application,
    Runtime,
}

/// A running writer's progress: the stage, and the graph it is fetching,
/// objects and bytes done out of what is known and of their bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub stage: Stage,
    pub part: Option<Part>,
    pub objects: u64,
    /// The objects known so far; it grows as the graph's trees are read.
    pub objects_known: u64,
    pub objects_bound: u64,
    pub bytes: u64,
    pub bytes_bound: u64,
}

impl Progress {
    pub fn idle(stage: Stage) -> Progress {
        Progress {
            stage,
            part: None,
            objects: 0,
            objects_known: 0,
            objects_bound: 0,
            bytes: 0,
            bytes_bound: 0,
        }
    }

    /// The record's one line, without its key: what `.work/progress` holds.
    pub fn render(&self) -> String {
        let part = match self.part {
            None => "-",
            Some(Part::Application) => "app",
            Some(Part::Runtime) => "runtime",
        };
        format!(
            "{}\t{part}\t{}\t{}\t{}\t{}\t{}",
            self.stage.word(),
            self.objects,
            self.objects_known,
            self.objects_bound,
            self.bytes,
            self.bytes_bound
        )
    }

    pub fn parse(line: &str) -> Result<Progress, String> {
        let fields: Vec<&str> = line.split('\t').collect();
        let [stage, part, objects, known, objects_bound, bytes, bytes_bound] = fields.as_slice()
        else {
            return Err(format!("progress {line:?} has the wrong fields"));
        };
        let stage = Stage::parse(stage).ok_or_else(|| format!("progress stage {stage:?}"))?;
        let part = match *part {
            "-" => None,
            "app" => Some(Part::Application),
            "runtime" => Some(Part::Runtime),
            other => return Err(format!("progress part {other:?}")),
        };
        let progress = Progress {
            stage,
            part,
            objects: decimal(objects)?,
            objects_known: decimal(known)?,
            objects_bound: decimal(objects_bound)?,
            bytes: decimal(bytes)?,
            bytes_bound: decimal(bytes_bound)?,
        };
        if progress.objects > progress.objects_known
            || progress.objects_known > progress.objects_bound
            || progress.bytes > progress.bytes_bound
        {
            return Err(format!("progress {line:?} exceeds its bounds"));
        }
        Ok(progress)
    }
}

/// Canonical unsigned decimal: no sign, no leading zero.
fn decimal(text: &str) -> Result<u64, String> {
    if text.is_empty()
        || (text.len() > 1 && text.starts_with('0'))
        || !text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!("{text:?} is not a canonical decimal"));
    }
    text.parse()
        .map_err(|_| format!("{text:?} is out of range"))
}

/// A refusal reason as a status carries it: printable ASCII, tabs and
/// everything else replaced, cut at [`MAX_REASON_BYTES`].
pub fn reason_text(message: &str) -> String {
    message
        .chars()
        .map(|c| if (' '..='~').contains(&c) { c } else { '?' })
        .take(MAX_REASON_BYTES)
        .collect()
}

fn valid_reason(reason: &str) -> bool {
    !reason.is_empty()
        && reason.len() <= MAX_REASON_BYTES
        && reason.bytes().all(|byte| (b' '..=b'~').contains(&byte))
}

impl Refusal {
    /// `refused`'s content in the cache: `TIME<TAB>REASON` and a newline.
    pub fn render(&self) -> String {
        format!("{}\t{}", self.time, self.reason)
    }

    pub fn parse(line: &str) -> Result<Refusal, String> {
        let (time, reason) = line
            .split_once('\t')
            .ok_or_else(|| "refusal has no reason".to_string())?;
        if !valid_reason(reason) {
            return Err("refusal reason is not bounded printable text".into());
        }
        Ok(Refusal {
            time: decimal(time)?,
            reason: reason.to_string(),
        })
    }
}

fn render_generation(text: &mut String, key: &str, generation: Option<&GenerationStatus>) {
    let Some(generation) = generation else {
        text.push_str(&format!("{key}=none\n"));
        return;
    };
    let serves = if generation.serves { "serves" } else { "stale" };
    text.push_str(&format!("{key}={}\t{serves}\n", generation.generation));
    for (part, deploy) in [
        ("app", &generation.application),
        ("runtime", &generation.runtime),
    ] {
        let bytes = deploy
            .bytes
            .map_or_else(|| "-".to_string(), |bytes| bytes.to_string());
        text.push_str(&format!(
            "{key}.{part}={}\t{}\t{}\t{bytes}\n",
            deploy.row.exact_ref, deploy.row.commit, deploy.row.timestamp
        ));
    }
}

fn parse_generation<'a>(
    lines: &mut impl Iterator<Item = &'a str>,
    key: &str,
) -> Result<Option<GenerationStatus>, String> {
    let head = status_field(lines.next(), key)?;
    if head == "none" {
        return Ok(None);
    }
    let (number, serves) = head
        .split_once('\t')
        .ok_or_else(|| format!("status {key} has no state"))?;
    let generation =
        generation_number(number).ok_or_else(|| format!("status {key} is no generation"))?;
    let serves = match serves {
        "serves" => true,
        "stale" => false,
        other => return Err(format!("status {key} state {other:?}")),
    };
    let mut deploy = |part: &str| -> Result<DeployStatus, String> {
        let row = status_field(lines.next(), &format!("{key}.{part}"))?;
        let (row, bytes) = row
            .rsplit_once('\t')
            .ok_or_else(|| format!("status {key}.{part} has no size"))?;
        let row = parse_row(row)?;
        require_kind(&row, part)?;
        let bytes = match bytes {
            "-" => None,
            bytes => Some(decimal(bytes)?),
        };
        Ok(DeployStatus { row, bytes })
    };
    let application = deploy("app")?;
    let runtime = deploy("runtime")?;
    Ok(Some(GenerationStatus {
        generation,
        serves,
        application,
        runtime,
    }))
}

fn status_field<'a>(line: Option<&'a str>, key: &str) -> Result<&'a str, String> {
    line.and_then(|line| line.strip_prefix(key))
        .and_then(|rest| rest.strip_prefix('='))
        .ok_or_else(|| format!("status lacks its {key}= line"))
}

impl Status {
    pub fn render(&self) -> String {
        let mut text = format!("{STATUS_FORMAT}\nname={}\n", self.name);
        let Some(cache) = &self.cache else {
            text.push_str("kind=image\n");
            return text;
        };
        text.push_str("kind=flathub\n");
        render_generation(&mut text, "current", cache.current.as_ref());
        render_generation(&mut text, "previous", cache.previous.as_ref());
        match cache.checked {
            Some(time) => text.push_str(&format!("checked={time}\n")),
            None => text.push_str("checked=never\n"),
        }
        match &cache.refused {
            Some(refusal) => text.push_str(&format!("refused={}\n", refusal.render())),
            None => text.push_str("refused=none\n"),
        }
        match &cache.progress {
            Some(progress) => text.push_str(&format!("progress={}\n", progress.render())),
            None => text.push_str("progress=none\n"),
        }
        text
    }

    /// Only the canonical rendering parses: a relay that re-renders what it
    /// parsed forwards exactly the bytes it checked.
    pub fn parse(text: &str) -> Result<Status, String> {
        if text.len() > MAX_STATUS_BYTES {
            return Err(format!("status exceeds {MAX_STATUS_BYTES} bytes"));
        }
        if !text.ends_with('\n') {
            return Err("status does not end in a newline".into());
        }
        let mut lines = text.split_terminator('\n');
        if lines.next() != Some(STATUS_FORMAT) {
            return Err("status has no format=1 header".into());
        }
        let name = status_field(lines.next(), "name")?;
        if !valid_name(name) {
            return Err(format!("status name {name:?} is malformed"));
        }
        let cache = match status_field(lines.next(), "kind")? {
            "image" => None,
            "flathub" => {
                let current = parse_generation(&mut lines, "current")?;
                let previous = parse_generation(&mut lines, "previous")?;
                let checked = match status_field(lines.next(), "checked")? {
                    "never" => None,
                    time => Some(decimal(time)?),
                };
                let refused = match status_field(lines.next(), "refused")? {
                    "none" => None,
                    line => Some(Refusal::parse(line)?),
                };
                let progress = match status_field(lines.next(), "progress")? {
                    "none" => None,
                    line => Some(Progress::parse(line)?),
                };
                Some(CacheStatus {
                    current,
                    previous,
                    checked,
                    refused,
                    progress,
                })
            }
            other => return Err(format!("status kind {other:?}")),
        };
        if let Some(line) = lines.next() {
            return Err(format!("status line {line:?} is out of place"));
        }
        let status = Status {
            name: name.to_string(),
            cache,
        };
        if status.render() != text {
            return Err("status is not canonical".into());
        }
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used)]

    use super::*;

    const COMMIT: &str = "7985364b35b83cfe5aa17a00932df1c2d1dae69e5c592aa637a53ff1d53fdabc";
    const RUNTIME_COMMIT: &str = "bd44a62300000000000000000000000000000000000000000000000000000000";
    const CHECKS: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn admitted(exact_ref: &str, commit: &str, timestamp: u64) -> Admitted {
        Admitted {
            exact_ref: exact_ref.into(),
            commit: commit.into(),
            timestamp,
        }
    }

    #[test]
    fn application_runtime_reads_only_the_application_group() {
        assert_eq!(
            application_runtime("[Application]\r\nruntime=a/b/c\r\n").unwrap(),
            "a/b/c"
        );
        assert!(application_runtime("[Runtime]\nruntime=a/b/c\n").is_err());
        assert!(application_runtime("[Application]\nruntime=a\nruntime=b\n").is_err());
        assert!(application_runtime("runtime=a/b/c\n[Application]\n").is_err());
        assert_eq!(
            application_runtime("[Application]\nruntime=a\n[Extension x]\nruntime=b\n").unwrap(),
            "a"
        );
        assert!(require_runtime("[Application]\nruntime=a\n", "app/a").is_err());
    }

    #[test]
    fn selections_round_trip_and_refuse_every_other_shape() {
        let app = admitted("app/org.mozilla.firefox/x86_64/stable", COMMIT, 20);
        let runtime = admitted(
            "runtime/org.freedesktop.Platform/x86_64/25.08",
            RUNTIME_COMMIT,
            10,
        );
        let fetched = Selection {
            name: "firefox".into(),
            checks: CHECKS.into(),
            application: app,
            runtime,
        };
        assert_eq!(Selection::parse(&fetched.render()).unwrap(), fetched);
        assert_eq!(
            fetched.commits().collect::<Vec<_>>(),
            vec![COMMIT, RUNTIME_COMMIT]
        );
        let text = fetched.render();
        let lines: Vec<&str> = text.lines().collect();
        let refused = [
            text.trim_end().to_string(),
            text.replacen("format=1", "format=2", 1),
            text.replacen("name=firefox", "name=Fire/fox", 1),
            text.replacen(CHECKS, "0011", 1),
            format!(
                "{}\n{}\n{}\n{}\n{}\n",
                lines[0], lines[2], lines[1], lines[3], lines[4]
            ),
            format!("{}\n{}\n{}\n{}\n", lines[0], lines[1], lines[2], lines[4]),
            format!("{text}{}\n", lines[4]),
            text.replacen("\t20", "\t020", 1),
            text.replacen("\t20", "\t", 1),
            text.replacen(COMMIT, &COMMIT.to_uppercase(), 1),
            text.replacen("app=app/", "app=runtime/", 1),
            text.replacen("runtime=runtime/", "runtime=app/", 1),
            text.replacen("x86_64/stable", "x86_64/../stable", 1),
            text.replacen("\t20", "\t20\textra", 1),
            // The retired runtime-only form: a store package in place of
            // the app row.
            text.replacen(
                lines[3],
                "package=/td/store/0123456789abcdefghijklmnopqrstuv-fixture-1",
                1,
            ),
            "x".repeat(MAX_STATE_BYTES + 1),
        ];
        for bad in refused {
            assert!(Selection::parse(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_spec_names_its_declared_refs_or_a_store_runtime() {
        let store = "/td/store/0123456789abcdfghijklmnpqrsvwxyz-freedesktop-platform-25-08";
        assert_eq!(spec_source("firefox", None, store), Ok(None));
        assert_eq!(spec_source("mail", None, store), Ok(None));
        let firefox = flathub_source("firefox").unwrap();
        let app = Some(firefox.application);
        assert_eq!(
            spec_source("firefox", app, firefox.runtime),
            Ok(Some(firefox))
        );
        for (name, source, runtime) in [
            ("firefox", app, store),
            // A runtime ref alone names no application tree.
            ("firefox", None, firefox.runtime),
            ("other", app, firefox.runtime),
            ("firefox", app, "runtime/org.example.Other/x86_64/1"),
            ("mail", None, firefox.runtime),
        ] {
            assert!(
                spec_source(name, source, runtime).is_err(),
                "{name} {source:?} {runtime}"
            );
        }
    }

    #[test]
    fn pointers_and_generation_names_are_exact() {
        assert_eq!(parse_pointer(&pointer_target(12)).unwrap(), 12);
        for bad in [
            "generations/0",
            "generations/01",
            "generations/1/",
            "/generations/1",
            "generations/x",
            "generations",
            "elsewhere/1",
            "generations/1/../2",
            "generations/99999999999999999999999",
        ] {
            assert!(parse_pointer(bad).is_err(), "{bad}");
        }
        assert_eq!(generation_number("7"), Some(7));
        assert_eq!(generation_number("07"), None);
    }

    #[test]
    fn refs_have_one_narrow_shape() {
        assert!(valid_ref("app/org.mozilla.firefox/x86_64/stable"));
        for bad in [
            "app/a/b",
            "app/a/b/c/d",
            "other/a/b/c",
            "app//b/c",
            "app/../b/c",
            "app/a b/c/d",
        ] {
            assert!(!valid_ref(bad), "{bad}");
        }
    }

    #[test]
    fn admitted_state_is_sorted_and_round_trips() {
        let rows = vec![
            admitted(
                "runtime/org.freedesktop.Platform/x86_64/25.08",
                RUNTIME_COMMIT,
                10,
            ),
            admitted("app/org.mozilla.firefox/x86_64/stable", COMMIT, 20),
        ];
        let text = render_admitted(&rows);
        assert!(text.starts_with("admitted=app/"));
        let parsed = parse_admitted(&text).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1], rows[0]);
        assert_eq!(parse_admitted("").unwrap(), Vec::new());
        let lines: Vec<&str> = text.lines().collect();
        for bad in [
            format!("{}\n{}\n", lines[1], lines[0]),
            format!("{}\n{}\n", lines[0], lines[0]),
            text.trim_end().to_string(),
            text.replacen("admitted=", "chosen=", 1),
        ] {
            assert!(parse_admitted(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn rule_three_refuses_older_and_same_time_other_commits() {
        let previous = admitted("app/a/x86_64/stable", COMMIT, 20);
        admits_after(None, RUNTIME_COMMIT, 0).unwrap();
        admits_after(Some(&previous), RUNTIME_COMMIT, 21).unwrap();
        admits_after(Some(&previous), COMMIT, 20).unwrap();
        assert!(admits_after(Some(&previous), RUNTIME_COMMIT, 20).is_err());
        assert!(admits_after(Some(&previous), RUNTIME_COMMIT, 19).is_err());
        assert!(admits_after(Some(&previous), COMMIT, 19).is_err());
    }

    #[test]
    fn the_floor_is_the_newest_row_for_its_ref() {
        let rows = [
            admitted("app/a/x86_64/stable", COMMIT, 20),
            admitted("app/a/x86_64/stable", RUNTIME_COMMIT, 30),
            admitted("runtime/a/x86_64/1", COMMIT, 40),
        ];
        assert_eq!(floor(&rows, "app/a/x86_64/stable").unwrap().timestamp, 30);
        assert_eq!(floor(&rows, "runtime/a/x86_64/1").unwrap().timestamp, 40);
        assert!(floor(&rows, "app/b/x86_64/stable").is_none());
    }

    fn deploy(exact_ref: &str, commit: &str, timestamp: u64, bytes: Option<u64>) -> DeployStatus {
        DeployStatus {
            row: admitted(exact_ref, commit, timestamp),
            bytes,
        }
    }

    #[test]
    fn statuses_round_trip_and_refuse_every_other_shape() {
        let image = Status {
            name: "mail".into(),
            cache: None,
        };
        assert_eq!(image.render(), "format=1\nname=mail\nkind=image\n");
        assert_eq!(Status::parse(&image.render()).unwrap(), image);
        let generation = |generation, serves| GenerationStatus {
            generation,
            serves,
            application: deploy(
                "app/org.mozilla.firefox/x86_64/stable",
                COMMIT,
                20,
                Some(300),
            ),
            runtime: deploy(
                "runtime/org.freedesktop.Platform/x86_64/25.08",
                RUNTIME_COMMIT,
                10,
                None,
            ),
        };
        let full = Status {
            name: "firefox".into(),
            cache: Some(CacheStatus {
                current: Some(generation(3, true)),
                previous: Some(generation(2, false)),
                checked: Some(1_800_000_000),
                refused: Some(Refusal {
                    time: 1_800_000_100,
                    reason: reason_text("rule 3:\tolder\ncommit é"),
                }),
                progress: Some(Progress {
                    stage: Stage::Fetch,
                    part: Some(Part::Runtime),
                    objects: 5,
                    objects_known: 9,
                    objects_bound: 300_000,
                    bytes: 4096,
                    bytes_bound: 1 << 31,
                }),
            }),
        };
        let text = full.render();
        assert!(
            text.contains("\nrefused=1800000100\trule 3:?older?commit ?\n"),
            "{text}"
        );
        assert!(text.contains("\ncurrent.runtime=runtime/org.freedesktop.Platform/x86_64/25.08\t"));
        assert!(text.contains("\t10\t-\n"));
        assert_eq!(Status::parse(&text).unwrap(), full);
        let empty = Status {
            name: "firefox".into(),
            cache: Some(CacheStatus {
                current: None,
                previous: None,
                checked: None,
                refused: None,
                progress: None,
            }),
        };
        assert_eq!(
            empty.render(),
            "format=1\nname=firefox\nkind=flathub\ncurrent=none\nprevious=none\n\
             checked=never\nrefused=none\nprogress=none\n"
        );
        assert_eq!(Status::parse(&empty.render()).unwrap(), empty);
        for stage in STAGES {
            let progress = Progress::idle(stage.0);
            assert_eq!(Progress::parse(&progress.render()).unwrap(), progress);
        }
        for bad in [
            text.replacen("kind=flathub", "kind=other", 1),
            text.replacen("current=3\tserves", "current=3\tfresh", 1),
            text.replacen("current=3", "current=03", 1),
            text.replacen("\t300\n", "\t0300\n", 1),
            text.replacen("\t300\n", "\t\n", 1),
            text.replacen("checked=1800000000", "checked=soon", 1),
            text.replacen("progress=fetch", "progress=dance", 1),
            text.replacen("\truntime\t5\t9", "\truntime\t10\t9", 1),
            text.replacen("\truntime\t5", "\tboth\t5", 1),
            text.replacen("\t4096\t", "\t4096\textra\t", 1),
            text.replacen("rule 3:", "", 1)
                .replacen("1800000100\t", "1800000100\t\t", 1),
            text.replacen("current.app=app/", "current.app=runtime/", 1),
            format!("{text}extra=1\n"),
            text.trim_end().to_string(),
            "x".repeat(MAX_STATUS_BYTES + 1),
            image.render().replacen("name=mail", "name=Mail", 1),
            format!("{}progress=none\n", image.render()),
        ] {
            assert!(Status::parse(&bad).is_err(), "{bad:?}");
        }
        let long = reason_text(&"x".repeat(MAX_REASON_BYTES * 2));
        assert_eq!(long.len(), MAX_REASON_BYTES);
        assert!(Refusal::parse(&format!("1\t{long}x")).is_err());
        assert!(Refusal::parse("1\t").is_err());
    }
}
