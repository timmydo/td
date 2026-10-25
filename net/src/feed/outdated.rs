// outdated — `td-feed outdated [--transitive] [pins|build-root|cargo]...`:
// say what upstream has released past td's pins (UPDATING.md).
//
// Three sections, all by default:
//   pins        each source pin's watch (`td-recipe-eval source-watches`):
//               an index page searched for the pin's pattern, newest wins
//   build-root  the Debian build root's image tag against the registry's
//               newest tag of the same shape, and each locked .deb against
//               the newest version the live archives list
//   cargo       every committed Cargo.lock's registry crates against the
//               crates.io sparse index: each direct dependency behind and
//               each yanked version by name, transitive ones counted
//               (`--transitive` lists them); a pinned crate's own lock
//               (`recipes/locks/*`) only as counts and yanks
//
// It is discovery only and changes nothing: what an index says steers a bump,
// and the bump still pins its bytes by SHA-256 in a reviewed diff. Exit 0 when
// everything read is current, 3 when something is behind, 1 when a watch
// could not be answered (which wins over 3). What no edit to the lock alone
// can move — a transitive crate whose only newer release is a new major, or
// anything in a pinned crate's own lock but a yank — is reported, not counted.
// A yank counts even there: it is a reason to look at the pin.
// The crate-wide grandfathered allow in main.rs does not cover this module.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)]
use super::{index_path, read_locked_cargo_sources, recipe_eval_stdout, LockedCargoSources};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const SECTIONS: &[&str] = &["pins", "build-root", "cargo"];
const BUILD_ROOT_LOCK: &str = "recipes/src/debian-root.lock";
/// crates.io itself: a mirror answering for it would say what the mirror has.
const CRATES_INDEX: &str = "https://index.crates.io";
/// A version whose release part names one of these is a preview.
const PRERELEASE: &[&str] = &["alpha", "beta", "rc", "pre", "dev"];
/// Index requests in flight at once; the cargo section asks for hundreds.
const WORKERS: usize = 8;

/// What one line of the report concluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Current,
    Behind,
    Failed,
}

#[derive(Default)]
struct Tally {
    behind: usize,
    failed: usize,
}

impl Tally {
    fn add(&mut self, status: Status) {
        match status {
            Status::Current => {}
            Status::Behind => self.behind += 1,
            Status::Failed => self.failed += 1,
        }
    }

    fn exit_code(&self) -> i32 {
        if self.failed > 0 {
            1
        } else if self.behind > 0 {
            3
        } else {
            0
        }
    }
}

pub(super) fn run(root: &Path, args: &[String]) -> Result<i32, String> {
    let transitive = args.iter().any(|arg| arg == "--transitive");
    let sections: Vec<&String> = args.iter().filter(|arg| *arg != "--transitive").collect();
    for arg in &sections {
        if !SECTIONS.contains(&arg.as_str()) {
            return Err(format!(
                "unknown section `{arg}' (usage: td-feed outdated [--transitive] \
                 [pins|build-root|cargo]...)"
            ));
        }
    }
    let wanted = |name: &str| sections.is_empty() || sections.iter().any(|arg| *arg == name);
    let mut tally = Tally::default();
    if wanted("pins") {
        pins(root, &mut tally)?;
    }
    if wanted("build-root") {
        build_root(root, &mut tally)?;
    }
    if wanted("cargo") {
        cargo(root, transitive, &mut tally)?;
    }
    println!(
        "td-feed: outdated: {} behind, {} could not be read",
        tally.behind, tally.failed
    );
    Ok(tally.exit_code())
}

fn is_version_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+')
}

/// A version's release part and its pre-release tag, if any. Build metadata
/// (`+…`) is dropped, and a `-` before a letter starts a pre-release, as
/// semver spells `1.0.0-rc.1`; a `-` before a digit (a date) does not.
fn split_release(version: &str) -> (&str, Option<&str>) {
    let core = version.split('+').next().unwrap_or(version);
    let bytes = core.as_bytes();
    let dash = (0..bytes.len()).find(|i| {
        bytes.get(*i) == Some(&b'-') && bytes.get(i + 1).is_some_and(u8::is_ascii_alphabetic)
    });
    match dash.and_then(|at| Some((core.get(..at)?, core.get(at + 1..)?))) {
        Some((release, tag)) => (release, Some(tag)),
        None => (core, None),
    }
}

fn is_release(version: &str) -> bool {
    let (release, tag) = split_release(version);
    let lower = release.to_ascii_lowercase();
    release.as_bytes().first().is_some_and(u8::is_ascii_digit)
        && tag.is_none()
        && !PRERELEASE.iter().any(|mark| lower.contains(mark))
}

/// Every release `pattern` names in `text`, where `{}` stands for a version:
/// the shortest run of version bytes that the rest of the pattern follows, or
/// the longest when nothing follows it. A line saying `"yanked":true` names
/// none (the crates.io sparse index keeps yanked versions as such lines).
fn releases(text: &str, pattern: &str) -> Vec<String> {
    let Some((prefix, suffix)) = pattern.split_once("{}") else {
        return Vec::new();
    };
    let mut found = Vec::new();
    if prefix.is_empty() {
        return found;
    }
    // Only a prefix that starts a word needs a word boundary before it.
    let word_start = prefix
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric());
    for line in text.lines() {
        if line.contains("\"yanked\":true") {
            continue;
        }
        let mut from = 0;
        while let Some(at) = line.get(from..).and_then(|rest| rest.find(prefix)) {
            let at = from + at;
            let Some(after) = line.get(at + prefix.len()..) else {
                break;
            };
            from = at + prefix.len();
            // A whole name: `git-` must not match inside `libgit-2.0`.
            let joined = word_start
                && line
                    .get(..at)
                    .and_then(|head| head.bytes().last())
                    .is_some_and(|b| b.is_ascii_alphanumeric());
            if joined {
                continue;
            }
            let run = after.bytes().take_while(|b| is_version_byte(*b)).count();
            let end = if suffix.is_empty() {
                Some(run)
            } else {
                (1..=run).find(|end| after.get(*end..).is_some_and(|t| t.starts_with(suffix)))
            };
            if let Some(version) = end.and_then(|end| after.get(..end)) {
                if is_release(version) {
                    found.push(version.to_string());
                }
            }
        }
    }
    found
}

/// Only versions of three or more numeric parts: a stable line's point
/// releases, not the fresh line itself (`Watch::PointReleases`).
fn point_releases(versions: Vec<String>) -> Vec<String> {
    versions
        .into_iter()
        .filter(|version| split_release(version).0.split('.').count() >= 3)
        .collect()
}

fn newest(versions: &[String]) -> Option<&String> {
    versions.iter().max_by(|a, b| compare(a, b))
}

/// Runs of digits compare as numbers, anything between them as text, and a
/// version that runs out first is the older: so 10.5p1 > 10.5, 2026e >
/// 2026d, 1.10 > 1.9 and 2026-10-20 > 2026-08-13. A pre-release orders
/// before its release (1.0.0-rc.1 < 1.0.0), and build metadata not at all.
fn compare(a: &str, b: &str) -> Ordering {
    let ((ra, ta), (rb, tb)) = (split_release(a), split_release(b));
    compare_pieces(ra, rb).then_with(|| match (ta, tb) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => compare_pieces(x, y),
    })
}

fn compare_pieces(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (pieces(a), pieces(b));
    loop {
        match (x.next(), y.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(p), Some(q)) => {
                let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
                let order = if digits(p) && digits(q) {
                    let (p, q) = (p.trim_start_matches('0'), q.trim_start_matches('0'));
                    p.len().cmp(&q.len()).then_with(|| p.cmp(q))
                } else {
                    p.cmp(q)
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

fn pieces(version: &str) -> impl Iterator<Item = &str> {
    let mut rest = version;
    std::iter::from_fn(move || {
        let digit = rest.bytes().next()?.is_ascii_digit();
        let len = rest
            .bytes()
            .take_while(|b| b.is_ascii_digit() == digit)
            .count();
        let (piece, tail) = rest.split_at_checked(len)?;
        rest = tail;
        Some(piece)
    })
}

/// Cargo's caret rule: the leftmost nonzero of major.minor.patch must agree,
/// so `cargo update` can reach `newer` without a manifest change.
fn semver_compatible(current: &str, newer: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        split_release(v)
            .0
            .split('.')
            .map(|n| n.parse().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parts(current), parts(newer));
    for i in 0..3 {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return false;
        }
        if x != 0 {
            return true;
        }
    }
    true
}

/// GET each URL once, a few at a time; the answer is the body or why not.
fn fetch_all(urls: BTreeSet<String>) -> BTreeMap<String, Result<String, String>> {
    let queue = Mutex::new(urls.into_iter().collect::<Vec<_>>());
    let answers = Mutex::new(BTreeMap::new());
    std::thread::scope(|scope| {
        for _ in 0..WORKERS {
            // A worker the OS refuses leaves the queue to the others; with
            // none, every URL reads as unanswered.
            let _ = std::thread::Builder::new().spawn_scoped(scope, || loop {
                let next = queue.lock().ok().and_then(|mut q| q.pop());
                let Some(url) = next else { break };
                let answer = crate::http::get_body(&url).and_then(|body| {
                    String::from_utf8(body).map_err(|e| format!("{url}: not UTF-8: {e}"))
                });
                if let Ok(mut answers) = answers.lock() {
                    answers.insert(url, answer);
                }
            });
        }
    });
    answers.into_inner().unwrap_or_default()
}

fn answer(bodies: &BTreeMap<String, Result<String, String>>, url: &str) -> Result<String, String> {
    bodies
        .get(url)
        .cloned()
        .unwrap_or_else(|| Err(format!("{url}: no answer")))
}

/// One `source-watches` row: KEY VERSION index|points INDEX PATTERN, or KEY
/// VERSION held REASON.
#[derive(Debug, PartialEq)]
enum Watch {
    Index {
        index: String,
        pattern: String,
        points: bool,
    },
    Held(String),
}

fn parse_watches(text: &str) -> Result<Vec<(String, String, Watch)>, String> {
    let mut rows = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let (key, version, watch) = match fields.as_slice() {
            [key, version, kind @ ("index" | "points"), index, pattern] => (
                key,
                version,
                Watch::Index {
                    index: (*index).to_string(),
                    pattern: (*pattern).to_string(),
                    points: *kind == "points",
                },
            ),
            [key, version, "held", reason] => (key, version, Watch::Held((*reason).to_string())),
            _ => return Err(format!("source-watches line {}: malformed: {line}", n + 1)),
        };
        rows.push((key.to_string(), version.to_string(), watch));
    }
    Ok(rows)
}

/// Judge a pinned version against the releases its index lists. A pin the
/// index no longer lists, with nothing newer, has been withdrawn or yanked,
/// which is as much a reason to move as a new release.
fn judge(current: &str, listed: &[String]) -> (Status, String) {
    let listed_current = listed
        .iter()
        .any(|v| compare(v, current) == Ordering::Equal);
    match newest(listed) {
        None => (Status::Failed, "lists no release".into()),
        Some(latest) if compare(latest, current) == Ordering::Greater => {
            let note = if listed_current {
                String::new()
            } else {
                format!(" (the index no longer lists {current})")
            };
            (Status::Behind, format!("-> {latest}{note}"))
        }
        Some(_) if listed_current => (Status::Current, "current".into()),
        Some(latest) => (
            Status::Behind,
            format!("not listed by its index (withdrawn?); its newest is {latest}"),
        ),
    }
}

fn pins(root: &Path, tally: &mut Tally) -> Result<(), String> {
    println!("source pins (recipes/src/source_pins.rs, recipes/src/source_watches.rs):");
    let rows = parse_watches(&recipe_eval_stdout(root, "source-watches")?)?;
    let urls = rows
        .iter()
        .filter_map(|(_, _, watch)| match watch {
            Watch::Index { index, .. } => Some(index.clone()),
            Watch::Held(_) => None,
        })
        .collect();
    let bodies = fetch_all(urls);
    let width = rows.iter().map(|(key, ..)| key.len()).max().unwrap_or(0);
    for (key, version, watch) in &rows {
        let (status, verdict) = match watch {
            Watch::Held(reason) => (Status::Current, format!("held: {reason}")),
            Watch::Index {
                index,
                pattern,
                points,
            } => match answer(&bodies, index) {
                Ok(body) => {
                    let mut listed = releases(&body, pattern);
                    if *points {
                        listed = point_releases(listed);
                    }
                    let (status, verdict) = judge(version, &listed);
                    if status == Status::Failed {
                        (status, format!("{index} {verdict} matching {pattern}"))
                    } else {
                        (status, verdict)
                    }
                }
                Err(error) => (Status::Failed, error),
            },
        };
        tally.add(status);
        println!("  {key:width$}  {version:12}  {verdict}");
    }
    Ok(())
}

/// A Docker Hub tag with a YYYYMMDD stamp in it, split around the stamp.
fn dated_tag(tag: &str) -> Option<(&str, &str, &str)> {
    let bytes = tag.as_bytes();
    let at = (0..bytes.len()).find(|i| {
        bytes
            .get(*i..i + 8)
            .is_some_and(|run| run.iter().all(u8::is_ascii_digit))
            && !bytes.get(i + 8).is_some_and(u8::is_ascii_digit)
            && (*i == 0 || !bytes.get(i - 1).is_some_and(u8::is_ascii_digit))
    })?;
    Some((tag.get(..at)?, tag.get(at..at + 8)?, tag.get(at + 8..)?))
}

/// The lock's `deb` rows as (package, version): `deb KEY FILE SHA SIZE URL
/// VERSION`, the package being FILE up to its first `_`.
fn locked_debs(lock: &str) -> Vec<(String, String)> {
    lock.lines()
        .filter_map(
            |line| match line.split_whitespace().collect::<Vec<_>>().as_slice() {
                ["deb", _, file, _, _, _, version] => {
                    let package = file.split('_').next()?;
                    Some((package.to_string(), (*version).to_string()))
                }
                _ => None,
            },
        )
        .collect()
}

fn build_root(root: &Path, tally: &mut Tally) -> Result<(), String> {
    println!("build root ({BUILD_ROOT_LOCK}):");
    let path = root.join(BUILD_ROOT_LOCK);
    let lock =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let image = lock
        .lines()
        .find_map(|line| line.strip_prefix("image "))
        .map(str::trim)
        .ok_or(format!("{BUILD_ROOT_LOCK}: no image line"))?;
    let (repository, tag) = image
        .split_once(':')
        .ok_or(format!("{BUILD_ROOT_LOCK}: image {image} has no tag"))?;
    let (head, stamp, tail) = dated_tag(tag).ok_or(format!(
        "{BUILD_ROOT_LOCK}: image tag {tag} carries no date"
    ))?;
    // Newest first, so the page holds the newest tags however many match.
    let tags = format!(
        "https://hub.docker.com/v2/repositories/{repository}/tags?name={head}\
         &ordering=last_updated&page_size=100"
    );
    let (status, verdict) = match answer(&fetch_all(BTreeSet::from([tags.clone()])), &tags) {
        Ok(body) => judge(
            stamp,
            &releases(&body, &format!("\"name\":\"{head}{{}}{tail}\"")),
        ),
        Err(error) => (Status::Failed, error),
    };
    tally.add(status);
    println!("  image {image}  {verdict}");

    let debs = locked_debs(&lock);
    let live = super::debian::lock_archives(&lock)
        .and_then(|archives| super::debian::live_versions(&archives));
    match live {
        Ok(live) => {
            let mut behind = 0usize;
            for (package, version) in &debs {
                let (status, verdict) = match live.get(package) {
                    Some(newer) if super::debian::is_newer(newer, version) => {
                        (Status::Behind, format!("-> {newer}"))
                    }
                    Some(_) => (Status::Current, String::new()),
                    None => (Status::Behind, "no longer in the archives".to_string()),
                };
                tally.add(status);
                if status != Status::Current {
                    behind += 1;
                    println!("    {package} {version} {verdict}");
                }
            }
            println!(
                "  {} locked debs, {} current against today's archives",
                debs.len(),
                debs.len() - behind
            );
        }
        Err(error) => {
            tally.add(Status::Failed);
            println!("  locked debs: {error}");
        }
    }
    Ok(())
}

/// The committed locks with registry crates: each roster crate's, td-net's
/// and the Rust userland recipes' (`recipes/locks/*`).
fn lock_paths(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    for dir in [root.to_path_buf(), root.join("recipes/locks")] {
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("read {}: {e}", dir.display()))?;
            let lock = entry.path().join("Cargo.lock");
            if entry.file_type().is_ok_and(|t| t.is_dir()) && lock.is_file() {
                paths.push(lock);
            }
        }
    }
    paths.sort();
    Ok(paths)
}

/// `recipes/locks/*` hold a pinned crate's own published Cargo.lock verbatim.
fn upstream_lock(shown: &str) -> bool {
    shown.starts_with("recipes/locks/")
}

/// What the crates.io index says about one locked version.
#[derive(Debug, Default, PartialEq)]
struct CrateFacts {
    yanked: bool,
    /// The newest semver-compatible release, when newer than the lock.
    compatible: Option<String>,
    /// The newest release of all, when newer than the lock and past a major.
    major: Option<String>,
}

/// A sparse-index line's string field, as crates.io writes it (`"k":"v"`).
fn index_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let start = line.find(&format!("\"{key}\":\""))? + key.len() + 4;
    let value = line.get(start..)?;
    value.get(..value.find('"')?)
}

/// What the crates.io index says about `locked`. A candidate must be a
/// semver release (any `-` before build metadata is a pre-release, which
/// Cargo never picks unasked) whose `rust_version` the pinned toolchain
/// meets, when `rust` names one.
fn crate_facts(locked: &str, index: &str, rust: Option<&str>) -> CrateFacts {
    let mut yanked = false;
    let mut listed = Vec::new();
    for line in index.lines() {
        let Some(vers) = index_field(line, "vers") else {
            continue;
        };
        if line.contains("\"yanked\":true") {
            yanked |= vers == locked;
            continue;
        }
        let semver_release =
            !split_release(vers).0.contains('-') && split_release(vers).1.is_none();
        let buildable = match (index_field(line, "rust_version"), rust) {
            (Some(needs), Some(have)) => compare(needs, have) != Ordering::Greater,
            _ => true,
        };
        if semver_release && is_release(vers) && buildable {
            listed.push(vers.to_string());
        }
    }
    let newer = |v: &&String| compare(v, locked) == Ordering::Greater;
    let compatible = listed
        .iter()
        .filter(newer)
        .filter(|v| semver_compatible(locked, v))
        .max_by(|a, b| compare(a, b))
        .cloned();
    let major = listed
        .iter()
        .filter(newer)
        .filter(|v| !semver_compatible(locked, v))
        .max_by(|a, b| compare(a, b))
        .cloned();
    CrateFacts {
        yanked,
        compatible,
        major,
    }
}

/// How one crate shows in the report.
#[derive(Debug, PartialEq)]
enum Shown {
    /// Its own line.
    Listed(String),
    /// Counted as transitive with a semver-compatible release.
    Within,
    /// Counted as transitive and only past a major.
    Past,
    Quiet,
}

/// Decide whether a crate counts toward the exit code and how it shows. A
/// yank always does, by name; a direct dependency does when anything is
/// newer; a transitive one when a semver-compatible release is newer; and
/// nothing in a pinned crate's own lock but a yank, since only its pin moves
/// it.
fn classify(facts: &CrateFacts, direct: bool, upstream: bool, transitive: bool) -> (Status, Shown) {
    let mut words = Vec::new();
    if facts.yanked {
        words.push("yanked".to_string());
    }
    if let Some(v) = &facts.compatible {
        words.push(format!("-> {v}"));
    }
    if let Some(v) = &facts.major {
        words.push(format!("-> {v} (major)"));
    }
    if direct {
        words.push("direct".to_string());
    }
    let line = words.join("  ");
    let newer = facts.compatible.is_some() || facts.major.is_some();
    if facts.yanked {
        return (Status::Behind, Shown::Listed(line));
    }
    if !newer {
        return (Status::Current, Shown::Quiet);
    }
    let counts = !upstream && (direct || facts.compatible.is_some());
    let status = if counts {
        Status::Behind
    } else {
        Status::Current
    };
    let shown = if transitive || (direct && !upstream) {
        Shown::Listed(line)
    } else if facts.compatible.is_some() {
        Shown::Within
    } else {
        Shown::Past
    };
    (status, shown)
}

fn is_direct(lock: &LockedCargoSources, name: &str, version: &str) -> bool {
    lock.direct.contains(&(name.to_string(), None))
        || lock
            .direct
            .contains(&(name.to_string(), Some(version.to_string())))
}

/// The pinned Rust toolchain's version, from its watch row.
fn pinned_rust(root: &Path) -> Option<String> {
    let rows = parse_watches(&recipe_eval_stdout(root, "source-watches").ok()?).ok()?;
    rows.into_iter()
        .find(|(key, ..)| key == "rust-rustc-dist")
        .map(|(_, version, _)| version)
}

fn cargo(root: &Path, transitive: bool, tally: &mut Tally) -> Result<(), String> {
    println!("cargo locks (crates.io):");
    let mut locks = Vec::new();
    for path in lock_paths(root)? {
        let (lock, _) = read_locked_cargo_sources(&path)?;
        if !lock.registry.is_empty() {
            let shown = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string();
            locks.push((shown, lock));
        }
    }
    let index = |name: &str| index_path(name).map(|path| format!("{CRATES_INDEX}/{path}"));
    let urls = locks
        .iter()
        .flat_map(|(_, lock)| lock.registry.iter())
        .filter_map(|package| index(&package.name))
        .collect();
    let bodies = fetch_all(urls);
    // Releases needing a newer Rust than the pinned toolchain do not count.
    let rust = pinned_rust(root);
    if rust.is_none() {
        tally.add(Status::Failed);
        println!("  the pinned Rust version is unreadable, so no rust_version is checked");
    }
    for (path, lock) in &locks {
        let upstream = upstream_lock(path);
        let (mut within, mut past, mut lines) = (0usize, 0usize, Vec::new());
        for package in &lock.registry {
            let (name, version) = (&package.name, &package.version);
            let body = match index(name) {
                Some(url) => answer(&bodies, &url),
                None => Err(format!("{name}: not a crates.io name")),
            };
            let (status, shown) = match body {
                Ok(body) if body.contains(&format!("\"vers\":\"{version}\"")) => classify(
                    &crate_facts(version, &body, rust.as_deref()),
                    is_direct(lock, name, version),
                    upstream,
                    transitive,
                ),
                // crates.io deletes a version only on request, soon after
                // publication: the lock names something no one can fetch.
                Ok(_) => (
                    Status::Behind,
                    Shown::Listed("withdrawn from crates.io".into()),
                ),
                Err(error) => (Status::Failed, Shown::Listed(error)),
            };
            tally.add(status);
            match shown {
                Shown::Listed(line) => lines.push(format!("    {name} {version}  {line}")),
                Shown::Within => within += 1,
                Shown::Past => past += 1,
                Shown::Quiet => {}
            }
        }
        let crates = lock.registry.len();
        if transitive {
            let note = if upstream {
                " (the crate's own published lock, which moves with its pin)"
            } else {
                ""
            };
            println!("  {path}: {crates} crates{note}");
        } else if upstream {
            println!(
                "  {path}: {crates} crates; not listed: {within} with a semver-compatible \
                 release, {past} only past a major (the crate's own published lock, which \
                 moves with its pin)"
            );
        } else {
            println!(
                "  {path}: {crates} crates; not listed: {within} transitive with a \
                 semver-compatible release, {past} only past a major"
            );
        }
        for line in lines {
            println!("{line}");
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn a_pattern_finds_each_release_and_skips_previews() {
        let listing = r#"<a href="curl-8.22.0.tar.xz">curl-8.22.0.tar.xz</a>
<a href="curl-8.22.0.tar.xz.asc">x</a> curl-8.23.0-rc1.tar.xz curl-8.23.0.tar.xz"#;
        assert_eq!(
            releases(listing, "curl-{}.tar.xz"),
            s(&["8.22.0", "8.22.0", "8.22.0", "8.23.0"])
        );
        // A Git server's ref advertisement: peeled tags and suffix-less patterns.
        let refs = "003fabc refs/tags/v3.5.0\n0042abc refs/tags/v3.5.1^{}\n\
                    0040abc refs/tags/v3.6.0-rc1\n0040abc refs/tags/v2.43-devel\n";
        assert_eq!(releases(refs, "refs/tags/v{}"), s(&["3.5.0", "3.5.1"]));
        // A name that is not a version (termcolor-1.0.0 under `refs/tags/{}`).
        assert_eq!(
            releases(
                "x refs/tags/termcolor-1.0.0\ny refs/tags/15.2.0\n",
                "refs/tags/{}"
            ),
            s(&["15.2.0"])
        );
        // A suffix that is itself made of version bytes.
        assert_eq!(
            releases(
                "url = \"https://x/rustc-1.99.1-x86_64-unknown-linux-gnu.tar.xz\"",
                "rustc-{}-x86_64-unknown-linux-gnu.tar.xz"
            ),
            s(&["1.99.1"])
        );
        assert_eq!(
            releases("refs/tags/json-c-0.19-20260627\n", "refs/tags/json-c-{}-"),
            s(&["0.19"])
        );
    }

    #[test]
    fn a_stable_watch_waits_for_a_lines_first_point_release() {
        let json = r#""source": "https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.3.tar.xz",
"source": "https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.2.12.tar.xz",
"source": "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.18.58.tar.xz","#;
        let listed = point_releases(releases(json, "linux-{}.tar.xz"));
        assert_eq!(listed, s(&["7.2.12", "6.18.58"]));
        assert_eq!(judge("7.2.12", &listed).0, Status::Current);
        // An end-of-life line drops out of releases.json.
        let (status, verdict) = judge("7.1.4", &listed);
        assert_eq!(status, Status::Behind);
        assert_eq!(verdict, "-> 7.2.12 (the index no longer lists 7.1.4)");
    }

    #[test]
    fn a_yanked_or_preview_crate_version_is_no_release() {
        let index = concat!(
            r#"{"name":"fd-find","vers":"10.4.0","yanked":false}"#,
            "\n",
            r#"{"name":"fd-find","vers":"10.5.0","yanked":true}"#,
            "\n",
            r#"{"name":"fd-find","vers":"11.0.0-beta.1","yanked":false}"#,
            "\n",
            r#"{"name":"wasi","vers":"0.11.1+wasi-snapshot-preview1","yanked":false}"#,
            "\n"
        );
        assert_eq!(
            releases(index, "\"vers\":\"{}\""),
            s(&["10.4.0", "0.11.1+wasi-snapshot-preview1"])
        );
    }

    #[test]
    fn versions_order_by_their_numbers() {
        let order = [
            ("10.5p1", "10.5", Ordering::Greater),
            ("10.6p1", "10.5p1", Ordering::Greater),
            ("2026e", "2026d", Ordering::Greater),
            ("1.10.0", "1.9.9", Ordering::Greater),
            ("2026-10-20", "2026-08-13", Ordering::Greater),
            ("20261020", "20261005", Ordering::Greater),
            ("1.3.2", "1.3.1.2", Ordering::Greater),
            ("1.0.0", "1.0.0-rc.1", Ordering::Greater),
            ("1.0.0-rc.2", "1.0.0-rc.1", Ordering::Greater),
            ("0.11.1+wasi-snapshot-preview1", "0.11.1", Ordering::Equal),
            ("7.1.4", "7.01.4", Ordering::Equal),
        ];
        for (a, b, want) in order {
            assert_eq!(compare(a, b), want, "{a} vs {b}");
            assert_eq!(compare(b, a), want.reverse(), "{b} vs {a}");
        }
        assert_eq!(
            newest(&s(&["4.3.4", "4.4.0", "4.10.0"])).map(String::as_str),
            Some("4.10.0")
        );
    }

    #[test]
    fn compatibility_follows_the_leftmost_nonzero_component() {
        assert!(semver_compatible("2.10.1", "2.12.0"));
        assert!(!semver_compatible("2.12.1", "3.0.0"));
        assert!(semver_compatible("0.10.8", "0.10.9"));
        assert!(!semver_compatible("0.10.8", "0.11.0"));
        assert!(!semver_compatible("0.0.3", "0.0.4"));
        assert!(semver_compatible("0.11.1+wasi-a", "0.11.2+wasi-b"));
    }

    #[test]
    fn a_verdict_says_behind_current_or_withdrawn() {
        assert_eq!(judge("1.0", &s(&["1.1", "1.0"])).0, Status::Behind);
        assert_eq!(judge("1.1", &s(&["1.1"])).0, Status::Current);
        assert_eq!(judge("1.2", &s(&["1.1"])).0, Status::Behind, "withdrawn");
        assert_eq!(judge("1.2", &[]).0, Status::Failed);
        assert_eq!(Tally::default().exit_code(), 0);
        let behind_and_failed = Tally {
            behind: 1,
            failed: 1,
        };
        assert_eq!(behind_and_failed.exit_code(), 1);
        assert_eq!(
            Tally {
                behind: 2,
                failed: 0
            }
            .exit_code(),
            3
        );
    }

    #[test]
    fn crate_facts_split_a_compatible_release_from_a_new_major() {
        let index = concat!(
            r#"{"vers":"2.0.118","yanked":false}"#,
            "\n",
            r#"{"vers":"2.0.119","yanked":false}"#,
            "\n",
            r#"{"vers":"3.0.9","yanked":false}"#,
            "\n",
            r#"{"vers":"0.5.13","yanked":true}"#,
            "\n"
        );
        assert_eq!(
            crate_facts("2.0.118", index, None),
            CrateFacts {
                yanked: false,
                compatible: Some("2.0.119".into()),
                major: Some("3.0.9".into()),
            }
        );
        assert!(crate_facts("0.5.13", index, None).yanked);
        assert_eq!(crate_facts("3.0.9", index, None), CrateFacts::default());
    }

    #[test]
    fn classification_counts_only_what_this_lock_can_move() {
        let within = CrateFacts {
            compatible: Some("1.0.1".into()),
            ..CrateFacts::default()
        };
        let major = CrateFacts {
            major: Some("2.0.0".into()),
            ..CrateFacts::default()
        };
        let yanked = CrateFacts {
            yanked: true,
            ..CrateFacts::default()
        };
        let current = CrateFacts::default();
        let status = |f: &CrateFacts, direct, upstream| classify(f, direct, upstream, false).0;
        let shown = |f: &CrateFacts, direct, upstream, all| classify(f, direct, upstream, all).1;
        // Direct: anything newer counts and is listed.
        assert_eq!(status(&major, true, false), Status::Behind);
        assert!(matches!(
            shown(&major, true, false, false),
            Shown::Listed(_)
        ));
        // Transitive: cargo update reaches it, or only a new major does.
        assert_eq!(status(&within, false, false), Status::Behind);
        assert_eq!(shown(&within, false, false, false), Shown::Within);
        assert_eq!(status(&major, false, false), Status::Current);
        assert_eq!(shown(&major, false, false, false), Shown::Past);
        assert!(matches!(
            shown(&major, false, false, true),
            Shown::Listed(_)
        ));
        // A pinned crate's own lock counts nothing but a yank.
        assert_eq!(status(&within, true, true), Status::Current);
        assert_eq!(shown(&within, true, true, false), Shown::Within);
        assert_eq!(status(&yanked, false, true), Status::Behind);
        assert!(matches!(
            shown(&yanked, false, true, false),
            Shown::Listed(_)
        ));
        assert_eq!(status(&current, true, false), Status::Current);
        assert_eq!(shown(&current, true, false, true), Shown::Quiet);
    }

    #[test]
    fn watch_rows_parse_and_refuse_a_malformed_line() {
        let text = "k\t1.0\tindex\thttps://x/\tk-{}.tar.xz\n\
                    p\t7.2.1\tpoints\thttps://y/\tl-{}.tar.xz\nh\t2\theld\tbecause\n";
        let rows = parse_watches(text).unwrap();
        assert_eq!(
            rows.iter().map(|(_, _, w)| w).collect::<Vec<_>>(),
            vec![
                &Watch::Index {
                    index: "https://x/".into(),
                    pattern: "k-{}.tar.xz".into(),
                    points: false
                },
                &Watch::Index {
                    index: "https://y/".into(),
                    pattern: "l-{}.tar.xz".into(),
                    points: true
                },
                &Watch::Held("because".into()),
            ]
        );
        assert!(parse_watches("k\t1.0\tsomething\n").is_err());
    }

    #[test]
    fn the_build_root_reads_its_tag_and_locked_debs() {
        assert_eq!(
            dated_tag("trixie-20261005-slim"),
            Some(("trixie-", "20261005", "-slim"))
        );
        assert_eq!(dated_tag("trixie-slim"), None);
        let hub = r#"{"results":[{"name":"trixie-20261020-slim"},{"name":"trixie-20261020"},{"name":"trixie-20261005-slim"}]}"#;
        assert_eq!(
            releases(hub, "\"name\":\"trixie-{}-slim\""),
            s(&["20261020", "20261005"])
        );
        let lock = "image library/debian:trixie-20261005-slim\n\
                    deb debian-root-libc6-dev libc6-dev_2.41-12+deb13u4_amd64.deb aa 1 https://x 2.41-12+deb13u4\n\
                    layer debian-root-layer f.tar.gz bb 2 https://y\n";
        assert_eq!(
            locked_debs(lock),
            vec![("libc6-dev".to_string(), "2.41-12+deb13u4".to_string())]
        );
    }

    #[test]
    fn a_lock_names_its_direct_dependencies_by_name_and_version() {
        let text = r#"version = 4

[[package]]
name = "td-net"
version = "0.1.0"
dependencies = [
 "ring",
 "webpki-roots 1.0.8",
]

[[package]]
name = "ring"
version = "0.17.14"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
dependencies = ["cc", "untrusted"]

[[package]]
name = "webpki-roots"
version = "0.26.11"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"

[[package]]
name = "webpki-roots"
version = "1.0.8"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
"#;
        let lock = super::super::parse_locked_cargo_sources(text).unwrap();
        assert_eq!(lock.registry.len(), 3);
        assert!(is_direct(&lock, "ring", "0.17.14"));
        assert!(is_direct(&lock, "webpki-roots", "1.0.8"));
        assert!(
            !is_direct(&lock, "webpki-roots", "0.26.11"),
            "the other one"
        );
        assert!(
            !is_direct(&lock, "cc", "1.0.0"),
            "a registry crate's own dependency"
        );
    }

    #[test]
    fn a_crate_candidate_is_a_semver_release_the_pinned_rust_builds() {
        let index = concat!(
            r#"{"vers":"1.0.0","yanked":false,"rust_version":"1.80"}"#,
            "\n",
            r#"{"vers":"1.0.1-1","yanked":false}"#,
            "\n",
            r#"{"vers":"1.0.2","yanked":false,"rust_version":"1.99.0"}"#,
            "\n",
            r#"{"vers":"1.0.3-rc.1+build.5","yanked":false}"#,
            "\n"
        );
        assert_eq!(
            crate_facts("1.0.0", index, Some("1.96.0")),
            CrateFacts::default()
        );
        assert_eq!(
            crate_facts("1.0.0", index, Some("1.99.1"))
                .compatible
                .as_deref(),
            Some("1.0.2")
        );
        assert_eq!(split_release("1.0.3-rc.1+build.5"), ("1.0.3", Some("rc.1")));
        assert_eq!(index_field(index, "rust_version"), Some("1.80"));
    }

    #[test]
    fn a_name_matches_whole_and_a_tail_must_follow() {
        assert_eq!(
            releases("libgit-2.0.tar.xz git-2.56.0.tar.xz", "git-{}.tar.xz"),
            s(&["2.56.0"])
        );
        // A second match judges the byte before it in the line, and a
        // prefix that starts with punctuation needs no boundary.
        assert_eq!(
            releases("git-1.0.tar.xz libgit-2.0.tar.xz", "git-{}.tar.xz"),
            s(&["1.0"])
        );
        assert_eq!(releases("x/v1.2/", "/v{}/"), s(&["1.2"]));
        // The non-slim tag's tail is not a version, so `trixie-{}` takes only
        // tags that end at the stamp.
        let hub = r#"{"name":"trixie-20261020-slim"},{"name":"trixie-20261020"}"#;
        assert_eq!(releases(hub, "\"name\":\"trixie-{}\""), s(&["20261020"]));
    }

    #[test]
    fn a_lock_dependency_entry_reads_in_each_form_cargo_writes() {
        use super::super::lock_dependency;
        assert_eq!(lock_dependency(" \"ring\","), Some(("ring".into(), None)));
        assert_eq!(
            lock_dependency("\"syn 2.0.118 (registry+https://example.invalid/index)\""),
            Some(("syn".into(), Some("2.0.118".into())))
        );
        let unclosed = "[[package]]\nname = \"a\"\nversion = \"1.0.0\"\n\
                        dependencies = [\n \"b\"]\n\n\
                        [[package]]\nname = \"b\"\nversion = \"1.0.0\"\n";
        let lock = super::super::parse_locked_cargo_sources(unclosed).unwrap();
        assert!(lock.direct.contains(&("b".to_string(), None)));
        assert!(super::super::parse_locked_cargo_sources(
            "[[package]]\nname = \"a\"\nversion = \"1\"\ndependencies = \"b\"\n"
        )
        .is_err());
    }
}
