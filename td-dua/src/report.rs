//! `td-dua report`: what a scan finds worth cleaning up, as text for a
//! person or JSON for an agent. Pure: the scan and the cache-tag reader
//! are handed in, and nothing here deletes.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};

use td_json::Json;

use crate::tree::{Kind, Measure, NodeId, Tree, MAX_NODES, ROOT};
use crate::view;

/// The most entries one section may list.
pub const MAX_TOP: usize = 10_000;

const DAY: i64 = 24 * 60 * 60;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Options {
    /// Entries per section.
    pub top: usize,
    /// A file unmodified for this many days is stale.
    pub stale_days: u64,
    /// The smallest file the file sections list.
    pub min_size: u64,
    pub measure: Measure,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            top: 20,
            stale_days: 365,
            min_size: 1024 * 1024,
            measure: Measure::Allocated,
        }
    }
}

/// Why a directory is a cleanup candidate: each names content its owner
/// usually recreates or has already discarded. They are heuristics, by
/// name or tag, for a reader to check before deleting.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Reason {
    /// Holds a `CACHEDIR.TAG` with the standard signature (Cargo's
    /// `target`, pytest's and mypy's caches, ccache among others).
    CacheTag,
    NodeModules,
    PyCache,
    /// A directory named `.cache` at any depth, the XDG cache home among
    /// them.
    CacheDir,
    /// The XDG trash at its default place, `.local/share/Trash`.
    Trash,
}

impl Reason {
    pub fn key(self) -> &'static str {
        match self {
            Reason::CacheTag => "cachedir-tag",
            Reason::NodeModules => "node-modules",
            Reason::PyCache => "pycache",
            Reason::CacheDir => "cache-dir",
            Reason::Trash => "trash",
        }
    }

    pub fn why(self) -> &'static str {
        match self {
            Reason::CacheTag => "marked a cache by CACHEDIR.TAG; its owner rebuilds it",
            Reason::NodeModules => "npm packages; an install recreates them unless vendored",
            Reason::PyCache => "Python bytecode; Python recreates it",
            Reason::CacheDir => "named .cache; programs usually refill it",
            Reason::Trash => "the desktop trash; already discarded",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub path: PathBuf,
    pub kind: Kind,
    pub bytes: u64,
    /// Entries beneath a directory that are not directories; 1 for any
    /// other entry but a mount, which is not entered.
    pub files: u64,
    /// The newest modification in the subtree, seconds since the epoch.
    pub modified: i64,
    /// A file whose inode has other names: deleting this one may free
    /// nothing.
    pub linked: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    pub entry: Entry,
    pub reason: Reason,
    /// Files beneath it whose inode has other names. Their bytes count
    /// where the scan met the inode first, maybe outside: deleting the
    /// candidate may free less or more than its total says.
    pub linked_files: u64,
    /// Other file systems mounted beneath it, which a recursive delete
    /// would enter.
    pub mounts: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Report {
    pub root: PathBuf,
    pub options: Options,
    /// The clock the report was built at, and the time before which a
    /// file is stale, seconds since the epoch.
    pub now: i64,
    pub stale_before: i64,
    pub bytes: u64,
    pub files: u64,
    pub directories: u64,
    pub partial: bool,
    /// The scan stopped at `tree::MAX_NODES` entries.
    pub node_limit: bool,
    /// Every candidate's bytes, listed or not.
    pub candidate_bytes: u64,
    pub candidate_count: u64,
    pub candidates: Vec<Candidate>,
    pub largest_files: Vec<Entry>,
    pub stale_files: Vec<Entry>,
    pub top_level: Vec<Entry>,
    pub unreadable_count: u64,
    pub unreadable: Vec<PathBuf>,
    pub mount_count: u64,
    pub mounts: Vec<PathBuf>,
}

/// The `top` first of what is offered, largest first and equal sizes by
/// path, each with its `T`. A min-heap of the kept: its least is the
/// smallest, and of equal sizes the last by path.
struct Largest<T> {
    top: usize,
    heap: BinaryHeap<Reverse<Kept<T>>>,
}

/// An offered entry's size, path (reversed, so equal sizes order by path
/// first), id and value.
type Kept<T> = (u64, Reverse<PathBuf>, NodeId, T);

impl<T: Ord> Largest<T> {
    fn new(top: usize) -> Self {
        Largest {
            top,
            heap: BinaryHeap::with_capacity(top.saturating_add(1)),
        }
    }

    fn offer(&mut self, bytes: u64, tree: &Tree, id: NodeId, value: T) {
        if self.top == 0 {
            return;
        }
        let full = self.heap.len() == self.top;
        let least = self.heap.peek().map(|Reverse(item)| item);
        if full && least.is_some_and(|least| bytes < least.0) {
            return;
        }
        let Some(path) = tree.path(id) else {
            return;
        };
        if full
            && least.is_some_and(|least| (bytes, Reverse(&path)) <= (least.0, Reverse(&least.1 .0)))
        {
            return;
        }
        self.heap.push(Reverse((bytes, Reverse(path), id, value)));
        if self.heap.len() > self.top {
            self.heap.pop();
        }
    }

    /// Largest first; equal sizes by path.
    fn into_sorted(self) -> Vec<(NodeId, T)> {
        let mut found: Vec<_> = self.heap.into_iter().map(|Reverse(item)| item).collect();
        found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| (a.1).0.cmp(&(b.1).0)));
        found
            .into_iter()
            .map(|(_, _, id, value)| (id, value))
            .collect()
    }

    fn into_ids(self) -> Vec<NodeId> {
        self.into_sorted().into_iter().map(|(id, _)| id).collect()
    }
}

/// Whether `id` names `.local/share/Trash`, by its whole path, since the
/// root node's name is the scanned path and may hold `.local` or `share`.
fn is_trash(tree: &Tree, id: NodeId) -> bool {
    tree.get(id).is_some_and(|node| node.name == "Trash")
        && tree
            .path(id)
            .is_some_and(|path| path.ends_with(".local/share/Trash"))
}

fn reason(tree: &Tree, id: NodeId, tagged: &mut dyn FnMut(&Path) -> bool) -> Option<Reason> {
    let node = tree.get(id)?;
    if node.kind != Kind::Dir || id == ROOT {
        return None;
    }
    if node.name == "node_modules" {
        return Some(Reason::NodeModules);
    }
    if node.name == "__pycache__" {
        return Some(Reason::PyCache);
    }
    if node.name == ".cache" {
        return Some(Reason::CacheDir);
    }
    if is_trash(tree, id) {
        return Some(Reason::Trash);
    }
    let tag = node.children.iter().copied().find(|&child| {
        tree.get(child)
            .is_some_and(|c| c.kind == Kind::File && c.name == "CACHEDIR.TAG")
    })?;
    let path = tree.path(tag)?;
    tagged(&path).then_some(Reason::CacheTag)
}

/// The linked files and the mounts beneath a candidate.
fn beneath(tree: &Tree, id: NodeId) -> (u64, u64) {
    let (mut linked, mut mounts) = (0u64, 0u64);
    for node in tree.subtree(id).into_iter().filter_map(|id| tree.get(id)) {
        if node.kind == Kind::Mount {
            mounts = mounts.saturating_add(1);
        } else if node.links {
            linked = linked.saturating_add(1);
        }
    }
    (linked, mounts)
}

fn entry(tree: &Tree, id: NodeId, measure: Measure) -> Option<Entry> {
    let node = tree.get(id)?;
    Some(Entry {
        path: tree.path(id)?,
        kind: node.kind,
        bytes: node.total.get(measure),
        files: match node.kind {
            Kind::Dir | Kind::Mount => node.files,
            Kind::File | Kind::Symlink | Kind::Other => 1,
        },
        modified: node.modified,
        linked: node.links,
    })
}

/// Builds the report over `tree` as of `now`, seconds since the epoch.
/// `tagged` says whether a `CACHEDIR.TAG` file carries the signature.
/// Candidates are the outermost: nothing beneath one is listed again,
/// neither as a candidate nor among the files, whose sections cover only
/// what lies outside every candidate.
pub fn build(
    tree: &Tree,
    options: Options,
    now: i64,
    tagged: &mut dyn FnMut(&Path) -> bool,
) -> Report {
    let measure = options.measure;
    let top = options.top.min(MAX_TOP);
    let stale_before = i64::try_from(options.stale_days)
        .unwrap_or(i64::MAX)
        .saturating_mul(DAY);
    let stale_before = now.saturating_sub(stale_before);
    let mut candidates = Largest::new(top);
    let mut largest = Largest::new(top);
    let mut stale = Largest::new(top);
    // Size plays no part in these two, so they keep the first paths.
    let mut unreadable = Largest::new(top);
    let mut mounts = Largest::new(top);
    let (mut directories, mut unreadable_count, mut mount_count) = (0u64, 0u64, 0u64);
    let (mut candidate_bytes, mut candidate_count) = (0u64, 0u64);

    // Each entry carries whether a candidate lies at or above it: the walk
    // goes on beneath one for the counts, but lists nothing there.
    let mut stack = vec![(ROOT, false)];
    while let Some((id, inside)) = stack.pop() {
        let Some(node) = tree.get(id) else {
            continue;
        };
        let bytes = node.total.get(measure);
        match node.kind {
            Kind::Dir => {
                directories = directories.saturating_add(1);
                if node.unreadable {
                    unreadable_count = unreadable_count.saturating_add(1);
                    unreadable.offer(0, tree, id, ());
                }
                let mut inside = inside;
                if !inside {
                    if let Some(why) = reason(tree, id, tagged) {
                        candidate_bytes = candidate_bytes.saturating_add(bytes);
                        candidate_count = candidate_count.saturating_add(1);
                        candidates.offer(bytes, tree, id, why);
                        inside = true;
                    }
                }
                stack.extend(node.children.iter().map(|&child| (child, inside)));
            }
            Kind::Mount => {
                mount_count = mount_count.saturating_add(1);
                mounts.offer(0, tree, id, ());
            }
            Kind::File if !inside => {
                if bytes >= options.min_size {
                    largest.offer(bytes, tree, id, ());
                    if node.modified < stale_before {
                        stale.offer(bytes, tree, id, ());
                    }
                }
            }
            Kind::File | Kind::Symlink | Kind::Other => {}
        }
    }

    let entries = |ids: Vec<NodeId>| -> Vec<Entry> {
        ids.into_iter()
            .filter_map(|id| entry(tree, id, measure))
            .collect()
    };
    let candidates = candidates
        .into_sorted()
        .into_iter()
        .filter_map(|(id, reason)| {
            let (linked_files, mounts) = beneath(tree, id);
            Some(Candidate {
                entry: entry(tree, id, measure)?,
                reason,
                linked_files,
                mounts,
            })
        })
        .collect();
    let mut top_level = Largest::new(top);
    for &child in tree.root().map_or(&[][..], |root| &root.children) {
        if let Some(node) = tree.get(child) {
            top_level.offer(node.total.get(measure), tree, child, ());
        }
    }
    let paths = |ids: Vec<NodeId>| -> Vec<PathBuf> {
        ids.into_iter().filter_map(|id| tree.path(id)).collect()
    };
    let root = tree.root();
    Report {
        root: tree.root_path().to_path_buf(),
        options: Options { top, ..options },
        now,
        stale_before,
        bytes: root.map_or(0, |root| root.total.get(measure)),
        files: root.map_or(0, |root| root.files),
        directories,
        partial: tree.partial,
        node_limit: tree.len() >= MAX_NODES,
        candidate_bytes,
        candidate_count,
        candidates,
        largest_files: entries(largest.into_ids()),
        stale_files: entries(stale.into_ids()),
        top_level: entries(top_level.into_ids()),
        unreadable_count,
        unreadable: paths(unreadable.into_ids()),
        mount_count,
        mounts: paths(mounts.into_ids()),
    }
}

/// A size argument: a whole number of bytes, or of KiB, MiB, GiB or TiB
/// with the suffix `K`, `M`, `G` or `T`.
pub fn parse_size(text: &str) -> Option<u64> {
    let (digits, shift) = match text.char_indices().last()? {
        (at, 'K' | 'k') => (text.get(..at)?, 10),
        (at, 'M' | 'm') => (text.get(..at)?, 20),
        (at, 'G' | 'g') => (text.get(..at)?, 30),
        (at, 'T' | 't') => (text.get(..at)?, 40),
        _ => (text, 0),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()?.checked_mul(1u64 << shift)
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::File => "file",
        Kind::Dir => "directory",
        Kind::Symlink => "symlink",
        Kind::Other => "other",
        Kind::Mount => "mount",
    }
}

/// `n thing` or `n things`, `directory` taking `directories`.
fn count(n: u64, thing: &str) -> String {
    match (n, thing.strip_suffix('y')) {
        (1, _) => format!("1 {thing}"),
        (_, Some(stem)) => format!("{n} {stem}ies"),
        (_, None) => format!("{n} {thing}s"),
    }
}

fn measure_name(measure: Measure) -> &'static str {
    match measure {
        Measure::Allocated => "allocated",
        Measure::Apparent => "apparent",
    }
}

/// A path as JSON: the exact string when it is UTF-8; otherwise lossy,
/// with `lossy` set so a reader knows not to act on it.
fn path_fields(path: &Path) -> Vec<(String, Json)> {
    let mut fields = vec![("path".to_owned(), Json::from(path.to_string_lossy()))];
    if path.to_str().is_none() {
        fields.push(("lossy".to_owned(), Json::from(true)));
    }
    fields
}

fn entry_json(entry: &Entry) -> Vec<(String, Json)> {
    let mut fields = path_fields(&entry.path);
    fields.extend([
        ("kind".to_owned(), Json::from(kind_name(entry.kind))),
        ("bytes".to_owned(), Json::from(entry.bytes)),
        ("files".to_owned(), Json::from(entry.files)),
        (
            "modified".to_owned(),
            Json::from(view::date(entry.modified)),
        ),
    ]);
    if entry.linked {
        fields.push(("linked".to_owned(), Json::from(true)));
    }
    fields
}

fn paths_json(paths: &[PathBuf]) -> Json {
    Json::Arr(
        paths
            .iter()
            .map(|path| Json::Obj(path_fields(path)))
            .collect(),
    )
}

fn entries_json(entries: &[Entry]) -> Json {
    Json::Arr(entries.iter().map(|e| Json::Obj(entry_json(e))).collect())
}

impl Report {
    /// One JSON document. Sizes are bytes in the report's measure; dates
    /// are UTC `YYYY-MM-DD`.
    pub fn to_json(&self) -> Json {
        let candidates = self
            .candidates
            .iter()
            .map(|candidate| {
                let mut fields = entry_json(&candidate.entry);
                fields.push(("reason".to_owned(), Json::from(candidate.reason.key())));
                fields.push(("why".to_owned(), Json::from(candidate.reason.why())));
                fields.push((
                    "linked_files".to_owned(),
                    Json::from(candidate.linked_files),
                ));
                fields.push(("mounts".to_owned(), Json::from(candidate.mounts)));
                Json::Obj(fields)
            })
            .collect();
        let mut fields = vec![("root".to_owned(), Json::from(self.root.to_string_lossy()))];
        if self.root.to_str().is_none() {
            fields.push(("root_lossy".to_owned(), Json::from(true)));
        }
        fields.extend([
            (
                "measure".to_owned(),
                Json::from(measure_name(self.options.measure)),
            ),
            ("bytes".to_owned(), Json::from(self.bytes)),
            ("files".to_owned(), Json::from(self.files)),
            ("directories".to_owned(), Json::from(self.directories)),
            ("partial".to_owned(), Json::from(self.partial)),
            ("node_limit".to_owned(), Json::from(self.node_limit)),
            ("now".to_owned(), Json::from(self.now)),
            ("top".to_owned(), Json::from(self.options.top)),
            ("stale_days".to_owned(), Json::from(self.options.stale_days)),
            ("stale_before".to_owned(), Json::from(self.stale_before)),
            ("min_size".to_owned(), Json::from(self.options.min_size)),
            (
                "candidate_bytes".to_owned(),
                Json::from(self.candidate_bytes),
            ),
            (
                "candidate_count".to_owned(),
                Json::from(self.candidate_count),
            ),
            ("candidates".to_owned(), Json::Arr(candidates)),
            (
                "largest_files".to_owned(),
                entries_json(&self.largest_files),
            ),
            ("stale_files".to_owned(), entries_json(&self.stale_files)),
            ("top_level".to_owned(), entries_json(&self.top_level)),
            (
                "unreadable_count".to_owned(),
                Json::from(self.unreadable_count),
            ),
            ("unreadable".to_owned(), paths_json(&self.unreadable)),
            ("mount_count".to_owned(), Json::from(self.mount_count)),
            ("mounts".to_owned(), paths_json(&self.mounts)),
        ]);
        Json::Obj(fields)
    }

    /// The report as lines for a terminal. Paths are shown as the list
    /// shows names, each control character a `?`, so a line is one entry.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        let line = |out: &mut String, text: &str| {
            out.push_str(text);
            out.push('\n');
        };
        let path = |path: &Path| view::display_name(path.as_os_str());
        line(
            &mut out,
            &format!(
                "td-dua report of {} ({} size)",
                path(&self.root),
                measure_name(self.options.measure)
            ),
        );
        line(
            &mut out,
            &format!(
                "Total {} in {} and {}.",
                view::size(self.bytes),
                count(self.files, "file"),
                count(self.directories, "directory")
            ),
        );
        if self.node_limit {
            line(
                &mut out,
                &format!("Partial: the scan stopped at its limit of {MAX_NODES} entries, so totals are low."),
            );
        }
        if self.unreadable_count > 0 {
            line(
                &mut out,
                &format!(
                    "Partial: {} could not be read, so totals are low.",
                    count(self.unreadable_count, "directory")
                ),
            );
        }
        line(&mut out, "");
        line(
            &mut out,
            &format!(
                "Cleanup candidates: {} in {}, each usually recreated by its owner or already discarded; check before deleting.",
                view::size(self.candidate_bytes),
                count(self.candidate_count, "directory")
            ),
        );
        for candidate in &self.candidates {
            let entry = &candidate.entry;
            line(
                &mut out,
                &format!(
                    "  {:>10}  {}  {:<12}  {}{}{}",
                    view::size(entry.bytes),
                    view::date(entry.modified),
                    candidate.reason.key(),
                    path(&entry.path),
                    if candidate.linked_files > 0 {
                        format!("  ({} linked)", count(candidate.linked_files, "file"))
                    } else {
                        String::new()
                    },
                    if candidate.mounts > 0 {
                        format!(
                            "  ({} mounted beneath)",
                            count(candidate.mounts, "file system")
                        )
                    } else {
                        String::new()
                    }
                ),
            );
        }
        let files = |out: &mut String, title: String, entries: &[Entry]| {
            line(out, "");
            line(out, &title);
            for entry in entries {
                line(
                    out,
                    &format!(
                        "  {:>10}  {}  {}{}",
                        view::size(entry.bytes),
                        view::date(entry.modified),
                        path(&entry.path),
                        if entry.linked { "  (linked)" } else { "" }
                    ),
                );
            }
        };
        let floor = view::size(self.options.min_size);
        files(
            &mut out,
            format!("Largest files outside the candidates, {floor} or more:"),
            &self.largest_files,
        );
        files(
            &mut out,
            format!(
                "Stale files outside the candidates, unmodified for {} days, {floor} or more:",
                self.options.stale_days
            ),
            &self.stale_files,
        );
        line(&mut out, "");
        line(&mut out, "Top level:");
        for entry in &self.top_level {
            let suffix = match entry.kind {
                Kind::Dir => "/",
                Kind::Mount => "  (other file system)",
                Kind::File | Kind::Symlink | Kind::Other => "",
            };
            line(
                &mut out,
                &format!(
                    "  {:>10}  {:>6}  {}{suffix}",
                    view::size(entry.bytes),
                    view::percent(entry.bytes, self.bytes),
                    path(&entry.path)
                ),
            );
        }
        for (title, count, paths) in [
            (
                "Unreadable directories",
                self.unreadable_count,
                &self.unreadable,
            ),
            (
                "Other file systems, not entered",
                self.mount_count,
                &self.mounts,
            ),
        ] {
            if count > 0 {
                line(&mut out, "");
                line(&mut out, &format!("{title}: {count}"));
                for entry in paths {
                    line(&mut out, &format!("  {}", path(entry)));
                }
            }
        }
        out
    }
}
