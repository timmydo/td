//! Flathub applications fetched on demand (APPLICATIONS.md §W.9): the
//! reviewed declarations, the per-account cache layout, and the pure halves
//! of admission.
//!
//! `td-app` (td-net) performs the I/O. Everything here is std-only, does
//! none and uses no other module, so td-jail can `#[path]`-include it to
//! read the same `selection` and pointers td-app writes.

/// One declared application's Flathub refs. The bytes are not reviewed;
/// these names, Flathub's signing key and the admission rules are.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlathubSource {
    /// The td application name: its registry row and launcher name.
    pub name: &'static str,
    /// The app ref, or `None` when only the runtime is fetched and the
    /// application tree stays in its `/td/store` package.
    pub application: Option<&'static str>,
    /// The one runtime ref the application accepts.
    pub runtime: &'static str,
    /// The application's entry inside its namespace.
    pub entry: &'static str,
    /// The runtime key of the application's dynamic-library policy.
    pub policy_runtime: &'static str,
}

pub const REPOSITORY: &str = "https://dl.flathub.org/repo";

const PLATFORM_25_08: &str = "runtime/org.freedesktop.Platform/x86_64/25.08";

pub const FLATHUB_SOURCES: &[FlathubSource] = &[
    FlathubSource {
        name: "claude",
        application: None,
        runtime: PLATFORM_25_08,
        entry: "/app/bin/claude",
        policy_runtime: "freedesktop-platform-25-08",
    },
    FlathubSource {
        name: "firefox",
        application: Some("app/org.mozilla.firefox/x86_64/stable"),
        runtime: PLATFORM_25_08,
        entry: "/app/bin/firefox",
        policy_runtime: "freedesktop-platform-25-08",
    },
];

pub fn flathub_source(name: &str) -> Option<FlathubSource> {
    FLATHUB_SOURCES
        .iter()
        .find(|source| source.name == name)
        .copied()
}

/// Where a spec's halves come from. A `/td/store` runtime path takes both
/// from the store and carries no `source=`; any other `runtime=` must be the
/// declaration's runtime ref, with `source=` exactly when it names an app
/// ref. The spec compiler and td-jail both hold specs to this.
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
    if runtime != declared.runtime || source != declared.application {
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
pub const WORK: &str = ".work";
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
    WORK,
];
/// A deploy's tree, below `deploy/<commit>/`.
pub const FILES: &str = "files";

pub const MAX_STATE_BYTES: usize = 4096;
const SELECTION_FORMAT: &str = "format=1";
const PACKAGE_ROOT: &str = "/td/store/";
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
    /// A digest of the rule-5 inputs no commit carries: the entry, the
    /// store package and the dynamic-library policy. td-app computes it.
    pub checks: String,
    /// The application commit, or `None` with `package` naming the
    /// `/td/store` package that supplies the application tree.
    pub application: Option<Admitted>,
    pub package: Option<String>,
    pub runtime: Admitted,
}

impl Selection {
    pub fn render(&self) -> String {
        let mut text = format!(
            "{SELECTION_FORMAT}\nname={}\nchecks={}\n",
            self.name, self.checks
        );
        if let Some(application) = &self.application {
            text.push_str(&render_row("app", application));
        }
        if let Some(package) = &self.package {
            text.push_str(&format!("package={package}\n"));
        }
        text.push_str(&render_row("runtime", &self.runtime));
        text
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
        let tree = lines.next().ok_or("selection names no application tree")?;
        let (application, package) = if let Some(row) = tree.strip_prefix("app=") {
            let admitted = parse_row(row)?;
            require_kind(&admitted, "app")?;
            (Some(admitted), None)
        } else {
            let package = field(Some(tree), "package")?;
            if !valid_package(package) {
                return Err(format!("selection package {package:?} is not a store path"));
            }
            (None, Some(package.to_string()))
        };
        let runtime = parse_row(field(lines.next(), "runtime")?)?;
        require_kind(&runtime, "runtime")?;
        if let Some(line) = lines.next() {
            return Err(format!("selection line {line:?} is out of place"));
        }
        Ok(Selection {
            name: name.to_string(),
            checks: checks.to_string(),
            application,
            package,
            runtime,
        })
    }

    /// The commits this generation keeps in the cache.
    pub fn commits(&self) -> impl Iterator<Item = &str> {
        self.rows().map(|admitted| admitted.commit.as_str())
    }

    pub fn rows(&self) -> impl Iterator<Item = &Admitted> {
        self.application
            .iter()
            .chain(std::iter::once(&self.runtime))
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

/// A direct child of `/td/store`.
pub fn valid_package(path: &str) -> bool {
    path.strip_prefix(PACKAGE_ROOT).is_some_and(|child| {
        !child.is_empty()
            && !child.starts_with('.')
            && child
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
    })
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

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used)]

    use super::*;

    const COMMIT: &str = "7985364b35b83cfe5aa17a00932df1c2d1dae69e5c592aa637a53ff1d53fdabc";
    const RUNTIME_COMMIT: &str = "bd44a62300000000000000000000000000000000000000000000000000000000";
    const CHECKS: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    const PACKAGE: &str = "/td/store/0123456789abcdefghijklmnopqrstuv-claude-1";

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
            application: Some(app),
            package: None,
            runtime: runtime.clone(),
        };
        let stored = Selection {
            name: "claude".into(),
            checks: CHECKS.into(),
            application: None,
            package: Some(PACKAGE.into()),
            runtime,
        };
        for selection in [&fetched, &stored] {
            assert_eq!(Selection::parse(&selection.render()).unwrap(), *selection);
        }
        assert_eq!(
            fetched.commits().collect::<Vec<_>>(),
            vec![COMMIT, RUNTIME_COMMIT]
        );
        assert_eq!(stored.commits().collect::<Vec<_>>(), vec![RUNTIME_COMMIT]);
        let text = fetched.render();
        let lines: Vec<&str> = text.lines().collect();
        let package_text = stored.render();
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
            package_text.replacen(PACKAGE, "/td/store/a/b", 1),
            package_text.replacen(PACKAGE, "/var/lib/x", 1),
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
        assert_eq!(
            spec_source("firefox", firefox.application, firefox.runtime),
            Ok(Some(firefox))
        );
        let claude = flathub_source("claude").unwrap();
        assert_eq!(
            spec_source("claude", None, claude.runtime),
            Ok(Some(claude))
        );
        for (name, source, runtime) in [
            ("firefox", firefox.application, store),
            ("firefox", None, firefox.runtime),
            ("claude", firefox.application, claude.runtime),
            (
                "firefox",
                firefox.application,
                "runtime/org.example.Other/x86_64/1",
            ),
            ("mail", None, claude.runtime),
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
    fn refs_and_packages_have_one_narrow_shape() {
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
        assert!(valid_package(PACKAGE));
        for bad in [
            "/td/store/",
            "/td/store/.x",
            "/td/store/a/b",
            "/td/storex/a",
        ] {
            assert!(!valid_package(bad), "{bad}");
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
}
