//! What each worktree's last run needed from the shared ladder build cache:
//! the roots `gc-store` keeps, in place of an age window.
//!
//! A run records, per worktree and per thing it built or checked, the store
//! basenames of every rung output its build named in a `STEP` line, as each is
//! named (a stopped or failed build's finished rungs stay, since the next
//! attempt reuses them), and the build-run or verdict memo it read or wrote.
//! The next completed run of the same key in the same worktree replaces that
//! record. GC keeps the closure of the union of every record and collects the
//! rest.
//!
//! A worktree is gone only when git says so: its directory is absent and so
//! is git's administrative directory for it, recorded beside its roots, from
//! a `worktrees/` registry that is itself visible. A sweep that cannot see the
//! registry (the check sandbox, a jail) keeps the record. A gone worktree's
//! record still roots until a live worktree's record of the same key is newer
//! or names all it names, and only the newest gone record of a key does.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

const DIR: &str = "gc-roots";
const HEADER: &str = "td-gc-roots 1";
/// Every version's header starts so; a version this evaluator does not know
/// is a newer evaluator's, which a sweep here must not judge.
const HEADER_PREFIX: &str = "td-gc-roots ";
const SUFFIX: &str = ".roots";
/// The memo directories under the ladder work dir a record may name.
pub(crate) const MEMO_DIRS: &[&str] = &["build-run-memo", "check-memo"];

/// One worktree's record for one key.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) worktree: PathBuf,
    /// Git's administrative directory for the worktree, when it is one.
    pub(crate) admin: Option<PathBuf>,
    pub(crate) outputs: BTreeSet<String>,
    /// Memo files as `<memo dir>/<file name>`.
    pub(crate) memos: BTreeSet<String>,
}

/// Every record a sweep keeps, merged.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Roots {
    pub(crate) records: usize,
    pub(crate) outputs: BTreeSet<String>,
    pub(crate) memos: BTreeSet<String>,
    /// Records dropped: a gone worktree's another worktree has replaced, an
    /// older gone one of the same key, or one that did not parse or was
    /// misfiled.
    pub(crate) dropped: usize,
}

/// The record key for a build of TARGET.
pub(crate) fn build_key(target: &str) -> String {
    format!("build-{}", safe(target))
}

/// How `record` treats what KEY's record already holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Update {
    /// Add to it: a build part way, a failed one, a memo beside a build.
    Merge,
    /// Replace it whole.
    Replace,
    /// Replace its outputs and keep its memos: a completed build, whose
    /// build-run memo the build-run path records under the same key.
    ReplaceOutputs,
}

/// The record key for a check's verdict memo.
pub(crate) fn check_key(stem: &str, index: usize) -> String {
    format!("check-{}.{index}", safe(stem))
}

fn safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn worktree_dir(lw: &Path, worktree: &Path) -> PathBuf {
    let mut h = crate::sha256::Sha256::new();
    h.update(worktree.as_os_str().as_encoded_bytes());
    let hex = crate::sha256::to_base16(&h.finalize());
    lw.join(DIR).join(hex.get(..32).unwrap_or(&hex))
}

/// Git's administrative directory for WORKTREE: the one a linked worktree's
/// `.git` file names, or a main checkout's `.git` directory itself.
pub(crate) fn admin_dir(worktree: &Path) -> Option<PathBuf> {
    let dot_git = worktree.join(".git");
    let meta = fs::symlink_metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    if !meta.is_file() {
        return None;
    }
    let text = fs::read_to_string(&dot_git).ok()?;
    let named = text.lines().next()?.strip_prefix("gitdir: ")?;
    // Relative under `worktree.useRelativePaths`; folded here, since once the
    // worktree is gone the filesystem can no longer resolve its `..`.
    let mut admin = PathBuf::new();
    for part in worktree.join(named).components() {
        match part {
            Component::ParentDir => {
                admin.pop();
            }
            Component::CurDir => {}
            other => admin.push(other),
        }
    }
    admin.is_absolute().then_some(admin)
}

/// Whether git has removed RECORD's worktree: the directory is absent, the
/// `worktrees/` registry that held its admin directory is visible, and that
/// admin directory is absent or, its name reused by a later worktree, names
/// another worktree in its `gitdir` file. A main checkout's record, or one
/// whose registry this process cannot see, is never gone.
fn gone(record: &Record) -> bool {
    let absent =
        |p: &Path| matches!(fs::symlink_metadata(p), Err(e) if e.kind() == io::ErrorKind::NotFound);
    let Some(admin) = &record.admin else {
        return false;
    };
    let Some(registry) = admin.parent() else {
        return false;
    };
    let reused = || {
        fs::read_to_string(admin.join("gitdir")).is_ok_and(|named| {
            let named = Path::new(named.trim_end());
            named.is_absolute() && named != record.worktree.join(".git")
        })
    };
    registry.file_name().is_some_and(|n| n == "worktrees")
        && fs::symlink_metadata(registry).is_ok_and(|m| m.is_dir())
        && absent(&record.worktree)
        && (absent(admin) || reused())
}

fn line(path: &Path) -> Option<&str> {
    path.to_str().filter(|s| !s.contains('\n'))
}

fn serialize(record: &Record) -> Option<String> {
    let mut text = format!("{HEADER}\nworktree {}\n", line(&record.worktree)?);
    if let Some(admin) = &record.admin {
        text.push_str(&format!("admin {}\n", line(admin)?));
    }
    for out in &record.outputs {
        text.push_str(&format!("out {out}\n"));
    }
    for memo in &record.memos {
        text.push_str(&format!("memo {memo}\n"));
    }
    Some(text)
}

fn basename_ok(base: &str) -> bool {
    !base.is_empty() && base != "." && base != ".." && !base.contains('/')
}

fn memo_ok(memo: &str) -> bool {
    memo.split_once('/')
        .is_some_and(|(dir, file)| MEMO_DIRS.contains(&dir) && basename_ok(file))
}

fn parse(text: &str) -> Option<Record> {
    let mut lines = text.lines();
    if lines.next()? != HEADER {
        return None;
    }
    let worktree = PathBuf::from(lines.next()?.strip_prefix("worktree ")?);
    if !worktree.is_absolute() {
        return None;
    }
    let mut record = Record {
        worktree,
        ..Record::default()
    };
    for line in lines {
        match line.split_once(' ')? {
            ("admin", admin) if record.admin.is_none() && Path::new(admin).is_absolute() => {
                record.admin = Some(PathBuf::from(admin));
            }
            ("out", base) if basename_ok(base) => {
                record.outputs.insert(base.to_string());
            }
            ("memo", memo) if memo_ok(memo) => {
                record.memos.insert(memo.to_string());
            }
            _ => return None,
        }
    }
    Some(record)
}

/// WORKTREE's record for KEY, or None when it has none or it does not parse.
pub(crate) fn read(lw: &Path, worktree: &Path, key: &str) -> Option<Record> {
    let path = worktree_dir(lw, worktree).join(format!("{}{SUFFIX}", safe(key)));
    parse(&fs::read_to_string(path).ok()?).filter(|r| r.worktree == worktree)
}

/// Replace the record for KEY in RECORD's worktree, unless it already says
/// the same: an unchanged record keeps its time, so it does not make a sweep
/// due. TAG names this run's temp, which a concurrent writer of the same key
/// never shares; the rename is the publish, so a reader sees one whole record
/// or the other.
pub(crate) fn write(lw: &Path, key: &str, record: &Record, tag: &str) -> Result<(), String> {
    let text = serialize(record).ok_or_else(|| {
        format!(
            "gc roots: {} is not a recordable path",
            record.worktree.display()
        )
    })?;
    let dir = worktree_dir(lw, &record.worktree);
    let path = dir.join(format!("{}{SUFFIX}", safe(key)));
    if fs::read_to_string(&path).is_ok_and(|old| old == text) {
        return Ok(());
    }
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".{}.{}.tmp", safe(key), safe(tag)));
    fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("rename {} -> {}: {e}", tmp.display(), path.display())
    })
}

/// Record OUTPUTS and MEMO (a file under LW) for KEY in the worktree at ROOT,
/// as UPDATE says.
pub(crate) fn record(
    lw: &Path,
    root: &Path,
    key: &str,
    outputs: impl IntoIterator<Item = String>,
    memo: Option<&Path>,
    update: Update,
    tag: &str,
) -> Result<(), String> {
    let worktree = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if !worktree.is_absolute() {
        return Ok(());
    }
    let mut record = match update {
        Update::Replace => Record::default(),
        Update::Merge | Update::ReplaceOutputs => read(lw, &worktree, key).unwrap_or_default(),
    };
    if update == Update::ReplaceOutputs {
        record.outputs.clear();
    }
    record.admin = admin_dir(&worktree);
    record.worktree = worktree;
    record.outputs.extend(outputs);
    if let Some(memo) = memo.and_then(|m| m.strip_prefix(lw).ok()) {
        record.memos.insert(memo.to_string_lossy().into_owned());
    }
    write(lw, key, &record, tag)
}

/// The record files, and the crash-left temps beside them.
fn listing(lw: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let root = lw.join(DIR);
    let mut records = Vec::new();
    let mut temps = Vec::new();
    let dirs = match fs::read_dir(&root) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((records, temps)),
        Err(e) => return Err(format!("read {}: {e}", root.display())),
    };
    for dir in dirs {
        let dir = dir
            .map_err(|e| format!("read {}: {e}", root.display()))?
            .path();
        if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        for entry in fs::read_dir(&dir).map_err(|e| format!("read {}: {e}", dir.display()))? {
            let path = entry
                .map_err(|e| format!("read {}: {e}", dir.display()))?
                .path();
            if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with('.') {
                if name.ends_with(".tmp") {
                    temps.push(path);
                }
            } else if name.ends_with(SUFFIX) {
                records.push(path);
            }
        }
    }
    Ok((records, temps))
}

/// When the newest record was written, or None when nothing is recorded.
pub(crate) fn newest(lw: &Path) -> Result<Option<SystemTime>, String> {
    let (records, _) = listing(lw)?;
    let mut newest = None;
    for path in records {
        let Ok(modified) = fs::metadata(&path).and_then(|m| m.modified()) else {
            continue;
        };
        newest = newest.max(Some(modified));
    }
    Ok(newest)
}

/// Merge every record that still roots, removing the rest: a gone worktree's
/// record once a live worktree's record of its key is at least as new or names all it
/// names, an older gone record of a key whose newest is gone too, and one
/// that does not parse or sits in another worktree's directory. A record of a version this evaluator does
/// not know stops the sweep with nothing removed. A dry run removes nothing.
/// Crash-left temps go too: the caller holds the ladder exclusive, and every
/// writer holds it shared or is a verdict-memo hit, whose record write is a
/// rename of its own temp a sweep may race only into a missing record for
/// that one run.
pub(crate) fn collect(lw: &Path, dry_run: bool) -> Result<Roots, String> {
    let (records, temps) = listing(lw)?;
    let mut kept = Vec::new();
    // Per key: the newest live record's time, and everything live records name.
    let mut live: BTreeMap<String, (SystemTime, Record)> = BTreeMap::new();
    let mut gone_by_key: BTreeMap<String, Vec<(SystemTime, PathBuf, Record)>> = BTreeMap::new();
    let mut drop = Vec::new();
    for path in records {
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("read {}: {e}", path.display())),
        };
        if let Some(header) = text.lines().next() {
            if header.starts_with(HEADER_PREFIX) && header != HEADER {
                return Err(format!(
                    "{}: `{header}` is another td-recipe-eval's record; nothing is \
                     collected until this one is as new",
                    path.display()
                ));
            }
        }
        let record =
            parse(&text).filter(|r| path.parent() == Some(worktree_dir(lw, &r.worktree).as_path()));
        let (Some(record), Some(key)) = (record, path.file_name()) else {
            drop.push(path);
            continue;
        };
        let key = key.to_string_lossy().into_owned();
        // A record is rewritten only when it changes, so this is when it last did.
        let written = fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if gone(&record) {
            gone_by_key
                .entry(key)
                .or_default()
                .push((written, path, record));
        } else {
            let (newest, names) = live
                .entry(key)
                .or_insert_with(|| (SystemTime::UNIX_EPOCH, Record::default()));
            *newest = (*newest).max(written);
            names.outputs.extend(record.outputs.iter().cloned());
            names.memos.extend(record.memos.iter().cloned());
            kept.push(record);
        }
    }
    for (key, mut gone) in gone_by_key {
        gone.sort_by_key(|(written, _, _)| *written);
        if let Some((written, path, newest)) = gone.pop() {
            let replaced = live.get(&key).is_some_and(|(live_written, names)| {
                *live_written >= written
                    || (newest.outputs.is_subset(&names.outputs)
                        && newest.memos.is_subset(&names.memos))
            });
            match replaced {
                true => drop.push(path),
                false => kept.push(newest),
            }
        }
        drop.extend(gone.into_iter().map(|(_, path, _)| path));
    }
    let mut roots = Roots {
        records: kept.len(),
        dropped: drop.len(),
        ..Roots::default()
    };
    for record in kept {
        roots.outputs.extend(record.outputs);
        roots.memos.extend(record.memos);
    }
    if dry_run {
        return Ok(roots);
    }
    for path in drop.iter().chain(&temps) {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("remove {}: {e}", path.display())),
        }
        // The worktree's last record takes its directory with it.
        if let Some(dir) = path.parent() {
            let _ = fs::remove_dir(dir);
        }
    }
    Ok(roots)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::test_temp::TempDir;

    fn record(worktree: &Path, outputs: &[&str], memos: &[&str]) -> Record {
        Record {
            worktree: worktree.to_path_buf(),
            admin: admin_dir(worktree),
            outputs: outputs.iter().map(|s| s.to_string()).collect(),
            memos: memos.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// A repository at ROOT/repo with linked worktrees at ROOT/NAME, laid out
    /// as git does: each `.git` file names `repo/.git/worktrees/NAME`.
    fn linked(root: &Path, name: &str, gitdir: Option<&str>) -> PathBuf {
        let admin = root.join("repo/.git/worktrees").join(name);
        fs::create_dir_all(&admin).unwrap();
        let tree = root.join(name);
        fs::create_dir_all(&tree).unwrap();
        let named = gitdir.map_or_else(|| admin.display().to_string(), str::to_string);
        fs::write(tree.join(".git"), format!("gitdir: {named}\n")).unwrap();
        tree
    }

    /// What `git worktree remove` leaves: neither the tree nor its admin dir.
    fn remove(root: &Path, name: &str) {
        fs::remove_dir_all(root.join(name)).unwrap();
        fs::remove_dir_all(root.join("repo/.git/worktrees").join(name)).unwrap();
    }

    #[test]
    fn a_record_round_trips_and_malformed_ones_do_not_parse() {
        let mut r = record(
            Path::new("/src/td"),
            &["aaaa-gcc-14", "bbbb-rustc"],
            &["build-run-memo/x.abc.map", "check-memo/y.0.k.pass"],
        );
        assert_eq!(parse(&serialize(&r).unwrap()).as_ref(), Some(&r));
        r.admin = Some(PathBuf::from("/src/td/.git/worktrees/x"));
        assert_eq!(parse(&serialize(&r).unwrap()), Some(r));
        for bad in [
            "",
            "td-gc-roots 2\nworktree /a\n",
            "td-gc-roots 1\nworktree relative\n",
            "td-gc-roots 1\nworktree /a\nadmin relative\n",
            "td-gc-roots 1\nworktree /a\nadmin /b\nadmin /c\n",
            "td-gc-roots 1\nworktree /a\nout a/b\n",
            "td-gc-roots 1\nworktree /a\nout ..\n",
            "td-gc-roots 1\nworktree /a\nmemo other/x\n",
            "td-gc-roots 1\nworktree /a\nmemo check-memo/a/b\n",
            "td-gc-roots 1\nworktree /a\nkeep x\n",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
        assert!(serialize(&record(Path::new("/a\nb"), &[], &[])).is_none());
    }

    #[test]
    fn keys_are_per_target_and_check() {
        assert_eq!(build_key("rust-x86-64"), "build-rust-x86-64");
        assert_eq!(check_key("td-sh/x", 2), "check-td-sh_x.2");
    }

    #[test]
    fn the_admin_dir_is_what_git_names_relative_or_not() {
        let owned = TempDir::new("td-gc-roots-admin").unwrap();
        let root = fs::canonicalize(owned.path()).unwrap();
        let admin = root.join("repo/.git/worktrees");
        let absolute = linked(&root, "a", None);
        assert_eq!(admin_dir(&absolute), Some(admin.join("a")));
        let relative = linked(&root, "b", Some("../repo/.git/worktrees/b"));
        assert_eq!(admin_dir(&relative), Some(admin.join("b")));
        fs::create_dir_all(root.join("main/.git")).unwrap();
        assert_eq!(admin_dir(&root.join("main")), Some(root.join("main/.git")));
        fs::create_dir_all(root.join("plain")).unwrap();
        assert_eq!(admin_dir(&root.join("plain")), None);
    }

    #[test]
    fn the_next_write_of_a_key_replaces_it_keys_add_up_and_a_same_write_is_no_write() {
        let owned = TempDir::new("td-gc-roots-write").unwrap();
        let lw = owned.join("lw");
        let tree = owned.join("tree");
        fs::create_dir_all(&tree).unwrap();
        assert_eq!(newest(&lw).unwrap(), None);
        record_into(&lw, &tree, "build-a", &["old-a"], false);
        record_into(&lw, &tree, "build-a", &["new-a"], false);
        record_into(&lw, &tree, "build-a", &["more-a"], true);
        // A completed build replaces its outputs and keeps the key's memo.
        let memo = lw.join("build-run-memo/b.f.map");
        let outs = |o: &[&str]| o.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        super::record(
            &lw,
            &tree,
            "build-b",
            outs(&["b-1"]),
            Some(&memo),
            Update::Merge,
            "t",
        )
        .unwrap();
        super::record(
            &lw,
            &tree,
            "build-b",
            outs(&["b-2"]),
            None,
            Update::ReplaceOutputs,
            "t",
        )
        .unwrap();
        let b = read(&lw, &tree, "build-b").unwrap();
        assert_eq!(b.outputs, BTreeSet::from(["b-2".to_string()]));
        assert_eq!(
            b.memos,
            BTreeSet::from(["build-run-memo/b.f.map".to_string()])
        );
        fs::remove_file(worktree_dir(&lw, &tree).join("build-b.roots")).unwrap();
        write(
            &lw,
            "check-c.0",
            &record(&tree, &[], &["check-memo/c.0.k.pass"]),
            "t3",
        )
        .unwrap();
        let roots = collect(&lw, false).unwrap();
        assert_eq!(roots.records, 2);
        assert_eq!(
            roots.outputs,
            BTreeSet::from(["more-a".to_string(), "new-a".to_string()])
        );
        assert_eq!(
            roots.memos,
            BTreeSet::from(["check-memo/c.0.k.pass".to_string()])
        );
        let path = worktree_dir(&lw, &tree).join("check-c.0.roots");
        let long_ago = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1 << 30);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        write(
            &lw,
            "check-c.0",
            &record(&tree, &[], &["check-memo/c.0.k.pass"]),
            "t4",
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            long_ago,
            "an unchanged record is not rewritten"
        );
    }

    fn record_into(lw: &Path, tree: &Path, key: &str, outputs: &[&str], merge: bool) {
        let outputs = outputs.iter().map(|s| s.to_string());
        let update = if merge {
            Update::Merge
        } else {
            Update::Replace
        };
        super::record(lw, tree, key, outputs, None, update, "t").unwrap();
    }

    #[test]
    fn only_a_worktree_git_removed_is_gone_and_its_key_roots_until_replaced() {
        let owned = TempDir::new("td-gc-roots-gone").unwrap();
        let root = fs::canonicalize(owned.path()).unwrap();
        let lw = root.join("lw");
        let live = linked(&root, "live", None);
        let old = linked(&root, "old", None);
        let older = linked(&root, "older", None);
        let covered = linked(&root, "covered", None);
        let moved = linked(&root, "moved", None);
        fs::create_dir_all(root.join("main/.git")).unwrap();
        let main = root.join("main");
        let at = |tree: &Path, key: &str, secs: u64| {
            let path = worktree_dir(&lw, tree).join(format!("{key}.roots"));
            let when = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(when)
                .unwrap();
        };
        record_into(&lw, &live, "build-shared", &["live-shared"], false);
        record_into(&lw, &live, "build-cov", &["cov", "more"], false);
        record_into(&lw, &old, "build-shared", &["old-shared"], false);
        record_into(&lw, &old, "build-own", &["old-own"], false);
        record_into(&lw, &older, "build-own", &["older-own"], false);
        record_into(&lw, &covered, "build-cov", &["cov"], false);
        record_into(&lw, &moved, "build-moved", &["moved"], false);
        record_into(&lw, &main, "build-main", &["main"], false);
        // Live's records predate the gone ones'; older's predates old's.
        at(&live, "build-shared", 1_000);
        at(&live, "build-cov", 1_000);
        at(&old, "build-shared", 2_000);
        at(&old, "build-own", 2_000);
        at(&older, "build-own", 1_500);
        at(&covered, "build-cov", 2_000);
        remove(&root, "old");
        remove(&root, "older");
        remove(&root, "covered");
        // Git reuses a freed admin name for a later worktree elsewhere.
        let reused = linked(&root, "reused", None);
        record_into(&lw, &reused, "build-reused", &["reused"], false);
        remove(&root, "reused");
        let admin = root.join("repo/.git/worktrees/reused");
        fs::create_dir_all(&admin).unwrap();
        fs::write(admin.join("gitdir"), "/elsewhere/reused/.git\n").unwrap();
        // Removed by hand: git still has its admin dir, so it is not gone.
        fs::write(
            root.join("repo/.git/worktrees/moved/gitdir"),
            format!("{}\n", moved.join(".git").display()),
        )
        .unwrap();
        fs::remove_dir_all(&moved).unwrap();
        fs::remove_dir_all(&main).unwrap();

        let all = |roots: &Roots| roots.outputs.iter().cloned().collect::<Vec<_>>();
        let dry = collect(&lw, true).unwrap();
        assert_eq!(
            all(&dry),
            [
                "cov",
                "live-shared",
                "main",
                "more",
                "moved",
                "old-own",
                "old-shared",
                "reused"
            ],
            "old's shared record is newer than live's, so it still roots; covered's \
             names nothing live's does not; older's own key is older than old's"
        );
        assert_eq!(
            dry.dropped, 2,
            "reused's record roots: no live record of its key"
        );
        assert!(
            gone(&read(&lw, &reused, "build-reused").unwrap()),
            "a reused admin name naming another worktree"
        );
        let older_own = worktree_dir(&lw, &older).join("build-own.roots");
        assert!(older_own.exists(), "a dry run removes nothing");

        // A sweep that cannot see the registry judges nothing gone.
        fs::rename(root.join("repo"), root.join("hidden")).unwrap();
        let blind = collect(&lw, true).unwrap();
        assert_eq!(blind.dropped, 0);
        assert_eq!(blind.records, 9);
        fs::rename(root.join("hidden"), root.join("repo")).unwrap();

        let swept = collect(&lw, false).unwrap();
        assert_eq!(all(&swept), all(&dry));
        assert!(!older_own.exists());
        assert!(
            !worktree_dir(&lw, &older).exists(),
            "its last record takes the directory"
        );
        assert!(!worktree_dir(&lw, &covered).exists());
        assert_eq!(collect(&lw, false).unwrap().dropped, 0);

        // Live builds the shared key again: old's record is replaced.
        record_into(&lw, &live, "build-shared", &["live-shared-2"], false);
        let replaced = collect(&lw, false).unwrap();
        assert_eq!(replaced.dropped, 1);
        assert!(!replaced.outputs.contains("old-shared"));
        assert!(replaced.outputs.contains("old-own"), "no live successor");
    }

    #[test]
    fn misfiled_and_corrupt_records_and_temps_are_removed_and_a_newer_version_stops_all() {
        let owned = TempDir::new("td-gc-roots-corrupt").unwrap();
        let lw = owned.join("lw");
        let tree = owned.join("tree");
        fs::create_dir_all(&tree).unwrap();
        write(&lw, "build-a", &record(&tree, &["a"], &[]), "t").unwrap();
        let dir = worktree_dir(&lw, &tree);
        fs::write(dir.join("bad.roots"), "not a record\n").unwrap();
        fs::write(dir.join(".build-a.dead.tmp"), "x").unwrap();
        let other = lw.join(DIR).join("0".repeat(32));
        fs::create_dir_all(&other).unwrap();
        fs::write(
            other.join("build-a.roots"),
            serialize(&record(&tree, &["x"], &[])).unwrap(),
        )
        .unwrap();
        fs::write(dir.join("newer.roots"), "td-gc-roots 2\nworktree /a\n").unwrap();
        let refused = collect(&lw, false).unwrap_err();
        assert!(refused.contains("td-gc-roots 2"), "{refused}");
        assert!(
            dir.join("bad.roots").exists(),
            "a refused sweep removes nothing"
        );
        fs::remove_file(dir.join("newer.roots")).unwrap();
        let roots = collect(&lw, false).unwrap();
        assert_eq!((roots.records, roots.dropped), (1, 2));
        assert_eq!(roots.outputs, BTreeSet::from(["a".to_string()]));
        assert!(!dir.join("bad.roots").exists());
        assert!(!dir.join(".build-a.dead.tmp").exists());
        assert!(!other.exists());
    }
}
