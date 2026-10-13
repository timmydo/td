//! Conservative reclamation of landed, idle linked worktrees.
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, SystemTime};

pub const DEFAULT_IDLE_DAYS: u64 = 7;

#[derive(Debug, Default)]
struct Worktree {
    path: PathBuf,
    head: String,
    branch: String,
    locked: bool,
    primary: bool,
}

fn command(repo: &Path, args: &[&str]) -> io::Result<std::process::Output> {
    Command::new("git")
        .current_dir(repo)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_SHALLOW_FILE")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "maintenance.auto=false",
        ])
        .args(args)
        .output()
}

fn git(repo: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let out = command(repo, args)?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(out.stdout)
}

fn text(repo: &Path, args: &[&str]) -> io::Result<String> {
    String::from_utf8(git(repo, args)?)
        .map(|s| s.trim().to_owned())
        .map_err(|e| io::Error::other(e.to_string()))
}

fn worktrees(repo: &Path) -> io::Result<Vec<Worktree>> {
    use std::os::unix::ffi::OsStringExt;
    let bytes = git(repo, &["worktree", "list", "--porcelain", "-z"])?;
    let mut all = Vec::new();
    let mut row = Worktree::default();
    for field in bytes.split(|b| *b == 0) {
        if field.is_empty() {
            if !row.path.as_os_str().is_empty() {
                row.primary = all.is_empty();
                all.push(std::mem::take(&mut row));
            }
        } else if let Some(path) = field.strip_prefix(b"worktree ") {
            row.path = std::ffi::OsString::from_vec(path.to_vec()).into();
        } else if let Some(head) = field.strip_prefix(b"HEAD ") {
            row.head = String::from_utf8_lossy(head).into_owned();
        } else if let Some(branch) = field.strip_prefix(b"branch ") {
            row.branch = String::from_utf8_lossy(branch).into_owned();
        } else if field == b"locked" || field.starts_with(b"locked ") {
            row.locked = true;
        }
    }
    if !row.path.as_os_str().is_empty() {
        row.primary = all.is_empty();
        all.push(row);
    }
    Ok(all)
}

// mtime and ctime include ignored build output and private Git activity.
// atime is excluded: inspecting directories must not make GC itself a use.
fn last_change(path: &Path) -> io::Result<SystemTime> {
    let meta = fs::symlink_metadata(path)?;
    let mut newest = meta.modified()?;
    if meta.ctime() >= 0 {
        let ctime =
            SystemTime::UNIX_EPOCH + Duration::new(meta.ctime() as u64, meta.ctime_nsec() as u32);
        newest = newest.max(ctime);
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            newest = newest.max(last_change(&entry?.path())?);
        }
    }
    Ok(newest)
}

fn under(target: &Path, roots: &[PathBuf]) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let bytes = target.as_os_str().as_encoded_bytes();
    let stripped = bytes.strip_suffix(b" (deleted)").unwrap_or(bytes);
    // The suffix is also legal literal path text: either interpretation
    // must keep a live process, including cwd at a root named that way.
    roots.iter().any(|root| {
        target.starts_with(root)
            || Path::new(std::ffi::OsStr::from_bytes(stripped)).starts_with(root)
    })
}

fn read_link_used(path: &Path, roots: &[PathBuf]) -> io::Result<bool> {
    match fs::read_link(path) {
        Ok(target) => Ok(under(&target, roots)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn check_proc_visibility(proc: &Path) -> io::Result<()> {
    if proc != Path::new("/proc") {
        return Ok(());
    }
    let mounts = fs::read_to_string(proc.join("self/mountinfo"))?;
    let row = mounts
        .lines()
        .find(|line| line.split_whitespace().nth(4) == Some("/proc"))
        .ok_or_else(|| io::Error::other("cannot establish procfs visibility"))?;
    if !row.contains(" - proc ")
        || row
            .split([' ', ','])
            .any(|option| option.starts_with("hidepid=") && option != "hidepid=0")
    {
        return Err(io::Error::other(
            "worktree GC requires procfs without hidepid restrictions",
        ));
    }
    Ok(())
}

fn thread_owned(status: &str, owner: u32) -> io::Result<bool> {
    let line = status
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .ok_or_else(|| io::Error::other("process status lacks Uid"))?;
    let ids: Vec<u32> = line
        .split_whitespace()
        .skip(1)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .map_err(io::Error::other)?;
    if ids.len() != 4 {
        return Err(io::Error::other("invalid process Uid record"));
    }
    Ok(ids.contains(&owner))
}

fn mapping_path(mut line: &str) -> &Path {
    for _ in 0..5 {
        line = line.trim_start();
        line = line
            .split_once(char::is_whitespace)
            .map(|(_, tail)| tail)
            .unwrap_or("");
    }
    Path::new(line.trim_start())
}

fn maps_root(path: &Path) -> PathBuf {
    // maps renders newline as \012 without escaping literal backslashes.
    // Encode our roots the same way; ambiguity can only retain extra trees.
    let mut bytes = Vec::new();
    for byte in path.as_os_str().as_encoded_bytes() {
        if *byte == b'\n' {
            bytes.extend_from_slice(b"\\012");
        } else {
            bytes.push(*byte);
        }
    }
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes).into()
}

fn process_uses(proc: &Path, roots: &[PathBuf], owner: u32) -> io::Result<bool> {
    let map_roots: Vec<PathBuf> = roots.iter().map(|root| maps_root(root)).collect();
    for entry in fs::read_dir(proc)? {
        let entry = entry?;
        if !entry
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        let pid = entry.path();
        let inspect = || -> io::Result<bool> {
            for thread in fs::read_dir(pid.join("task"))? {
                let thread = thread?.path();
                let inspect_thread = || -> io::Result<bool> {
                    let status = fs::read_to_string(thread.join("status"))?;
                    if !thread_owned(&status, owner)? {
                        return Ok(false);
                    }
                    if status.lines().any(|line| {
                        line.starts_with("State:") && line.split_whitespace().nth(1) == Some("Z")
                    }) {
                        return Ok(false);
                    }
                    for name in ["cwd", "root", "exe"] {
                        if read_link_used(&thread.join(name), roots)? {
                            return Ok(true);
                        }
                    }
                    for link in fs::read_dir(thread.join("fd"))? {
                        if read_link_used(&link?.path(), roots)? {
                            return Ok(true);
                        }
                    }
                    for line in fs::read_to_string(thread.join("maps"))?.lines() {
                        if under(mapping_path(line), &map_roots) {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                };
                match inspect_thread() {
                    Ok(true) => return Ok(true),
                    Ok(false) => (),
                    Err(e) if e.kind() == io::ErrorKind::NotFound && !thread.exists() => (),
                    Err(e) => return Err(e),
                }
            }
            Ok(false)
        };
        match inspect() {
            Ok(true) => return Ok(true),
            Ok(false) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound && !pid.exists() => (),
            Err(e) => return Err(io::Error::other(format!("inspect {}: {e}", pid.display()))),
        }
    }
    Ok(false)
}

fn eligible(
    repo: &Path,
    row: &Worktree,
    main: &str,
    idle: Duration,
    now: SystemTime,
    proc: &Path,
) -> io::Result<Option<String>> {
    if row.primary {
        return Ok(Some("primary checkout".into()));
    }
    if row.locked {
        return Ok(Some("locked".into()));
    }
    if row.branch.ends_with("-rolling") {
        return Ok(Some("rolling workstream".into()));
    }
    if row.branch == "refs/heads/main" {
        return Ok(Some("main branch".into()));
    }
    if !fs::symlink_metadata(row.path.join(".git"))?.is_file() {
        return Ok(Some("primary checkout or invalid Git marker".into()));
    }
    if fs::canonicalize(repo)?.starts_with(fs::canonicalize(&row.path)?) {
        return Ok(Some("current checkout".into()));
    }
    let actual = text(&row.path, &["rev-parse", "HEAD"])?;
    if actual != row.head {
        return Ok(Some("HEAD changed".into()));
    }
    let ancestor = command(repo, &["merge-base", "--is-ancestor", &actual, main])?;
    match ancestor.status.code() {
        Some(0) => (),
        Some(1) => return Ok(Some("commits absent from origin/main".into())),
        _ => return Err(io::Error::other("cannot establish ancestry")),
    }
    if !git(
        &row.path,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )?
    .is_empty()
    {
        return Ok(Some("uncommitted or untracked files".into()));
    }
    let admin = PathBuf::from(text(&row.path, &["rev-parse", "--absolute-git-dir"])?);
    if fs::canonicalize(text(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?)? != fs::canonicalize(text(
        &row.path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?)? {
        return Ok(Some("Git repository changed".into()));
    }
    for marker in [
        "rebase-merge",
        "rebase-apply",
        "sequencer",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_LOG",
        "index.lock",
    ] {
        if admin.join(marker).exists() {
            return Ok(Some("Git operation in progress".into()));
        }
    }
    let history = text(&row.path, &["reflog", "show", "--format=%H", "HEAD"])?;
    let private = text(
        &row.path,
        &[
            "for-each-ref",
            "--format=%(objectname)",
            "refs/worktree",
            "refs/rewritten",
            "refs/bisect",
        ],
    )?;
    // Removing a linked tree keeps the branch and its common-directory
    // reflog. Amendments there are not unique private worktree history.
    let retained = if row.branch.is_empty() {
        String::new()
    } else {
        text(repo, &["reflog", "show", "--format=%H", &row.branch])?
    };
    let retained: std::collections::HashSet<&str> = retained.lines().collect();
    let mut seen = std::collections::HashSet::new();
    for oid in history.lines().chain(private.lines()) {
        if retained.contains(oid) || !seen.insert(oid) {
            continue;
        }
        let result = command(repo, &["merge-base", "--is-ancestor", oid, main])?;
        match result.status.code() {
            Some(0) => (),
            Some(1) => return Ok(Some("unlanded private Git history".into())),
            _ => {
                return Err(io::Error::other(
                    "cannot establish private history ancestry",
                ))
            }
        }
    }
    let newest = last_change(&row.path)?.max(last_change(&admin)?);
    if now.duration_since(newest).unwrap_or_default() < idle {
        return Ok(Some("recent activity".into()));
    }
    if process_uses(
        proc,
        &[fs::canonicalize(&row.path)?, fs::canonicalize(admin)?],
        fs::metadata(&row.path)?.uid(),
    )? {
        return Ok(Some("live process uses checkout or Git metadata".into()));
    }
    Ok(None)
}

pub fn sweep(repo: &Path, days: u64, dry: bool, fetch: bool) -> io::Result<()> {
    sweep_inner(repo, days, dry, fetch, Path::new("/proc"))
}

fn sweep_inner(repo: &Path, days: u64, dry: bool, fetch: bool, proc: &Path) -> io::Result<()> {
    let seconds = days
        .checked_mul(86400)
        .ok_or_else(|| io::Error::other("idle days overflow"))?;
    let common = PathBuf::from(text(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?);
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(common.join("td-worktree-gc.lock"))?;
    lock.try_lock()
        .map_err(|e| io::Error::other(format!("worktree GC already running: {e}")))?;
    if fetch {
        git(
            repo,
            &[
                "fetch",
                "--no-tags",
                "origin",
                "+refs/heads/main:refs/remotes/origin/main",
            ],
        )?;
    }
    check_proc_visibility(proc)?;
    let main = text(
        repo,
        &["rev-parse", "--verify", "refs/remotes/origin/main^{commit}"],
    )?;
    // Shallow history cannot establish that all old commits landed.
    if text(repo, &["rev-parse", "--is-shallow-repository"])? != "false" {
        return Err(io::Error::other("worktree GC requires complete history"));
    }
    if common.join("info/grafts").exists() {
        return Err(io::Error::other("worktree GC refuses grafted history"));
    }
    let idle = Duration::from_secs(seconds);
    for row in worktrees(repo)? {
        let decide = || eligible(repo, &row, &main, idle, SystemTime::now(), proc);
        match decide() {
            Ok(None) if dry => println!("would remove {}", row.path.display()),
            Ok(None) => {
                // Re-list locks and recheck everything immediately before Git's
                // own clean/unlocked removal. Never force deletion.
                let fresh = worktrees(repo)?.into_iter().find(|r| r.path == row.path);
                let Some(fresh) = fresh else {
                    continue;
                };
                match eligible(repo, &fresh, &main, idle, SystemTime::now(), proc) {
                    Ok(None) if fresh.head == row.head => {
                        let Some(path) = row.path.to_str() else {
                            println!("keep {}: non-UTF-8 removal path", row.path.display());
                            continue;
                        };
                        match git(repo, &["worktree", "remove", "--", path]) {
                            Ok(_) => println!("removed {}", row.path.display()),
                            Err(e) => println!("keep {}: removal refused: {e}", row.path.display()),
                        }
                    }
                    _ => println!("keep {}: changed during inspection", row.path.display()),
                }
            }
            Ok(Some(why)) => println!("keep {}: {why}", row.path.display()),
            Err(e) => println!("keep {}: safety check failed: {e}", row.path.display()),
        }
    }
    Ok(())
}

pub fn cli(args: &[String]) -> ExitCode {
    let run = || -> io::Result<()> {
        let mut days = DEFAULT_IDLE_DAYS;
        let mut dry = false;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--dry-run" if !dry => dry = true,
                "--idle-days" => {
                    days = args
                        .next()
                        .ok_or_else(|| io::Error::other("missing idle days"))?
                        .parse()
                        .map_err(io::Error::other)?
                }
                _ => {
                    return Err(io::Error::other(
                        "usage: td-builder worktree-gc [--idle-days N] [--dry-run]",
                    ))
                }
            }
        }
        sweep(&std::env::current_dir()?, days, dry, true)
    };
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("td-builder worktree-gc: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::test_temp::TempDir;
    use std::os::unix::fs::symlink;

    fn fixture() -> (TempDir, PathBuf, Worktree, PathBuf, String) {
        let root = TempDir::new("td-worktree-gc-test").unwrap();
        let repo = root.join("repo");
        let tree = root.join("tree with spaces");
        let proc = root.join("proc");
        fs::create_dir(&repo).unwrap();
        fs::create_dir(&proc).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        git(&repo, &["config", "user.name", "GC test"]).unwrap();
        git(&repo, &["config", "user.email", "gc@example.invalid"]).unwrap();
        fs::write(repo.join("file"), "committed").unwrap();
        git(&repo, &["add", "file"]).unwrap();
        git(&repo, &["commit", "-m", "base"]).unwrap();
        let main = text(&repo, &["rev-parse", "HEAD"]).unwrap();
        git(&repo, &["update-ref", "refs/remotes/origin/main", &main]).unwrap();
        git(
            &repo,
            &["worktree", "add", "-b", "landed", tree.to_str().unwrap()],
        )
        .unwrap();
        let row = worktrees(&repo)
            .unwrap()
            .into_iter()
            .find(|r| r.path == tree)
            .unwrap();
        (root, repo, row, proc, main)
    }

    fn reason(repo: &Path, row: &Worktree, main: &str, proc: &Path) -> Option<String> {
        eligible(repo, row, main, Duration::ZERO, SystemTime::now(), proc).unwrap()
    }

    #[test]
    fn ancestry_clean_lock_and_age_are_each_required() {
        let (_root, repo, mut row, proc, main) = fixture();
        assert_eq!(reason(&repo, &row, &main, &proc), None);
        row.locked = true;
        assert_eq!(reason(&repo, &row, &main, &proc).as_deref(), Some("locked"));
        row.locked = false;
        fs::write(row.path.join("untracked"), "precious").unwrap();
        assert_eq!(
            reason(&repo, &row, &main, &proc).as_deref(),
            Some("uncommitted or untracked files")
        );
        fs::remove_file(row.path.join("untracked")).unwrap();
        fs::write(row.path.join("file"), "dirty").unwrap();
        assert!(reason(&repo, &row, &main, &proc).is_some());
        git(&row.path, &["add", "file"]).unwrap();
        git(&row.path, &["commit", "-m", "unlanded"]).unwrap();
        row.head = text(&row.path, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(
            reason(&repo, &row, &main, &proc).as_deref(),
            Some("commits absent from origin/main")
        );
        let main = row.head.clone();
        assert_eq!(reason(&repo, &row, &main, &proc), None);
        assert_eq!(
            eligible(
                &repo,
                &row,
                &main,
                Duration::from_secs(86400),
                SystemTime::now(),
                &proc
            )
            .unwrap()
            .as_deref(),
            Some("recent activity")
        );
        assert_eq!(
            eligible(
                &repo,
                &row,
                &main,
                Duration::from_secs(86400),
                SystemTime::now() + Duration::from_secs(172800),
                &proc
            )
            .unwrap(),
            None
        );
        row.branch = "refs/heads/work-rolling".into();
        assert!(reason(&repo, &row, &main, &proc).is_some());
    }

    #[test]
    fn cwd_open_files_and_mapped_files_keep_a_tree() {
        let (_root, repo, row, proc, main) = fixture();
        let pid = proc.join("123/task/123");
        fs::create_dir_all(pid.join("fd")).unwrap();
        fs::write(
            pid.join("status"),
            format!(
                "Uid:\t{0} {0} {0} {0}\nState:\tS\n",
                fs::metadata(&row.path).unwrap().uid()
            ),
        )
        .unwrap();
        fs::write(pid.join("maps"), "").unwrap();
        for link in ["cwd", "fd/8"] {
            symlink(row.path.join("file"), pid.join(link)).unwrap();
            assert_eq!(
                reason(&repo, &row, &main, &proc).as_deref(),
                Some("live process uses checkout or Git metadata")
            );
            fs::remove_file(pid.join(link)).unwrap();
        }
        fs::write(
            pid.join("maps"),
            format!(
                "100-200 rw-p 00000000 00:00 0 {}\n",
                row.path.join("file").display()
            ),
        )
        .unwrap();
        assert!(reason(&repo, &row, &main, &proc).is_some());
        fs::write(pid.join("maps"), "").unwrap();
        // Lexical siblings do not count as descendants.
        symlink(row.path.with_extension("other"), pid.join("cwd")).unwrap();
        assert_eq!(reason(&repo, &row, &main, &proc), None);
        // Missing/uninspectable process surfaces refuse deletion.
        fs::remove_dir(pid.join("fd")).unwrap();
        assert!(eligible(&repo, &row, &main, Duration::ZERO, SystemTime::now(), &proc).is_err());
    }

    #[test]
    fn private_history_and_interrupted_operations_are_retained() {
        let (_root, repo, mut row, proc, main) = fixture();
        let admin = PathBuf::from(text(&row.path, &["rev-parse", "--absolute-git-dir"]).unwrap());
        fs::write(admin.join("index.lock"), "held").unwrap();
        assert_eq!(
            reason(&repo, &row, &main, &proc).as_deref(),
            Some("Git operation in progress")
        );
        fs::remove_file(admin.join("index.lock")).unwrap();
        git(&row.path, &["checkout", "--detach"]).unwrap();
        row.branch.clear();
        fs::write(row.path.join("file"), "unlanded").unwrap();
        git(&row.path, &["commit", "-am", "private"]).unwrap();
        git(&row.path, &["reset", "--hard", &main]).unwrap();
        row.head = main.clone();
        assert_eq!(
            reason(&repo, &row, &main, &proc).as_deref(),
            Some("unlanded private Git history")
        );
        fs::create_dir(row.path.join("subdir")).unwrap();
        assert_eq!(
            reason(&row.path.join("subdir"), &row, &main, &proc).as_deref(),
            Some("current checkout")
        );
    }

    #[test]
    fn retained_branch_reflog_keeps_superseded_commits_after_gc() {
        let (_root, repo, row, proc, main) = fixture();
        fs::write(row.path.join("file"), "draft").unwrap();
        git(&row.path, &["commit", "-am", "draft"]).unwrap();
        let draft = text(&row.path, &["rev-parse", "HEAD"]).unwrap();
        git(&row.path, &["reset", "--hard", &main]).unwrap();
        assert_eq!(reason(&repo, &row, &main, &proc), None);
        sweep_inner(&repo, 0, false, false, &proc).unwrap();
        assert!(!row.path.exists());
        assert!(text(
            &repo,
            &["reflog", "show", "--format=%H", "refs/heads/landed"]
        )
        .unwrap()
        .lines()
        .any(|oid| oid == draft));
        git(&repo, &["cat-file", "-e", &format!("{draft}^{{commit}}")]).unwrap();
    }

    #[test]
    fn real_owner_process_with_cwd_in_tree_is_detected() {
        let (_root, _repo, row, proc, _main) = fixture();
        let mut child = Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .current_dir(&row.path)
            .spawn()
            .unwrap();
        // Inspect our child through real procfs, independent of unrelated
        // host processes remapped to our UID by the test runner's namespace.
        symlink(
            format!("/proc/{}", child.id()),
            proc.join(child.id().to_string()),
        )
        .unwrap();
        let used = process_uses(
            &proc,
            &[row.path.clone()],
            fs::metadata(&row.path).unwrap().uid(),
        );
        drop(child.stdin.take());
        child.wait().unwrap();
        assert!(used.unwrap());
    }

    #[test]
    fn foreign_threads_are_skipped_and_map_names_preserve_spaces_and_newlines() {
        assert!(!thread_owned("Uid:\t0 0 0 0\n", 1001).unwrap());
        assert!(thread_owned("Uid:\t0 1001 0 0\n", 1001).unwrap());
        assert!(thread_owned("Uid:\t1001 0 0 0\n", 1001).unwrap());
        assert_eq!(
            mapping_path("0-1 r--p 0 0:0 0 /tmp/name with spaces\\012line"),
            Path::new("/tmp/name with spaces\\012line")
        );
    }

    #[test]
    fn deleted_suffix_and_maps_escapes_retain_both_interpretations() {
        let literal = PathBuf::from("/tmp/tree (deleted)");
        assert!(under(&literal, &[literal.clone()]));
        assert!(under(&literal, &[PathBuf::from("/tmp/tree")]));
        let roots = [
            PathBuf::from("/tmp/tree\npart\\012end"),
            PathBuf::from("/tmp/tree\\012part\\012end"),
        ];
        let encoded: Vec<PathBuf> = roots.iter().map(|root| maps_root(root)).collect();
        assert!(under(
            mapping_path("0-1 r--p 0 0:0 0 /tmp/tree\\012part\\012end/file"),
            &encoded
        ));
    }

    #[test]
    fn linked_and_primary_worktrees_and_real_git_removal() {
        let (_root, repo, row, proc, main) = fixture();
        let primary = worktrees(&repo)
            .unwrap()
            .into_iter()
            .find(|r| r.path == repo)
            .unwrap();
        assert!(reason(&repo, &primary, &main, &proc).is_some());
        assert_eq!(reason(&repo, &row, &main, &proc), None);
        sweep_inner(&repo, 0, true, false, &proc).unwrap();
        assert!(row.path.exists());
        sweep_inner(&repo, 0, false, false, &proc).unwrap();
        assert!(!row.path.exists());
        assert!(repo.exists());
    }
}
