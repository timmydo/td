//! Disposable, commit-pinned workspaces for command-line reviews.

use std::collections::BTreeSet;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::{git, host, jail, repo, review, tools, workspace};

const GIT_TIME: Duration = Duration::from_secs(120);
const TOOL_TIME: Duration = Duration::from_secs(660);
const MAX_PATHS: usize = 128;
const MAX_TOOL_REPLY: usize = 256 * 1024;

pub struct Workspace {
    root: PathBuf,
    cleaned: bool,
    _lock: File,
    pub checkout: PathBuf,
    pub scratch: PathBuf,
    pub commit: String,
    pub subject: String,
    pub diff: String,
    pub sparse: Vec<String>,
    programs: jail::Programs,
    repository: PathBuf,
    objects: PathBuf,
    git: PathBuf,
}

fn directory(path: &Path) -> Result<(), String> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
            let own = fs::metadata("/proc/self").map_err(|e| e.to_string())?.uid();
            if !meta.is_dir() || meta.uid() != own || meta.mode() & 0o077 != 0 {
                return Err(format!(
                    "{} is not a private owned directory",
                    path.display()
                ));
            }
            Ok(())
        }
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn lock(path: &Path) -> Result<File, String> {
    // The parent is private and never mounted into a jail.
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(0o400000)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// A stopped owner releases its lock even after SIGKILL. Only its private
/// run directory is collected; a symlink is never traversed.
fn sweep(base: &Path) -> Result<(), String> {
    let serial = lock(&base.join("sweep.lock"))?;
    serial.lock().map_err(|e| e.to_string())?;
    for entry in fs::read_dir(base).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("run-") {
            continue;
        }
        if !entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            continue;
        }
        let held = match lock(&entry.path().join("owner.lock")) {
            Ok(held) => held,
            Err(why) => {
                eprintln!(
                    "td-agent review: cannot collect {}: {why}",
                    entry.path().display()
                );
                continue;
            }
        };
        if held.try_lock().is_ok() {
            if let Err(why) = workspace::remove_tree(&entry.path()) {
                eprintln!(
                    "td-agent review: cannot collect {}: {why}",
                    entry.path().display()
                );
            }
        }
    }
    Ok(())
}

fn command(git: &Path, dir: &Path) -> Command {
    let mut command = Command::new(git);
    command
        .env_clear()
        .stdin(Stdio::null())
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("--no-pager");
    for setting in [
        "core.hooksPath=/dev/null",
        "core.fsmonitor=false",
        "gc.auto=0",
        "maintenance.auto=false",
        "protocol.allow=never",
    ] {
        command.args(["-c", setting]);
    }
    command
}

fn said(command: &mut Command, limit: u64) -> Result<String, String> {
    let bytes = git::run(command, limit, GIT_TIME).map_err(|e| e.to_string())?;
    String::from_utf8(bytes).map_err(|_| "git returned non-UTF-8 text".into())
}

impl Workspace {
    pub fn prepare(
        source: &Path,
        revision: &str,
        sparse: &[String],
        key_path: &Path,
    ) -> Result<Self, String> {
        let programs = jail::Programs::from_env()?;
        let git = repo::host_git()?;
        let source = fs::canonicalize(source).map_err(|e| format!("{}: {e}", source.display()))?;
        let commit = said(
            command(&git, &source).args([
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{revision}^{{commit}}"),
            ]),
            256,
        )?
        .trim()
        .to_string();
        if !git::object_id(&commit) {
            return Err("git did not resolve a full commit id".into());
        }
        let common = said(
            command(&git, &source).args([
                "rev-parse",
                "--path-format=absolute",
                "--git-common-dir",
            ]),
            8192,
        )?;
        let common = fs::canonicalize(common.trim()).map_err(|e| e.to_string())?;
        let parent = key_path.parent().ok_or("the key has no parent directory")?;
        let base = parent.join("reviews");
        directory(&base)?;
        sweep(&base)?;
        // Hold the sweep lock through creation and locking, so a concurrent
        // review never collects a directory whose owner is still starting.
        let serial = lock(&base.join("sweep.lock"))?;
        serial.lock().map_err(|e| e.to_string())?;
        let root = base.join(format!("run-{}", crate::store::random_hex(16)?));
        DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .map_err(|e| e.to_string())?;
        let held = lock(&root.join("owner.lock"))?;
        held.lock().map_err(|e| e.to_string())?;
        drop(serial);
        let mut result = Self {
            checkout: root.join("source"),
            scratch: root.join("scratch"),
            repository: root.join("repository.git"),
            objects: common.join("objects"),
            root,
            cleaned: false,
            _lock: held,
            commit,
            subject: String::new(),
            diff: String::new(),
            sparse: Vec::new(),
            programs,
            git,
        };
        directory(&result.scratch)?;
        directory(&result.root.join("home"))?;
        directory(&result.root.join("specs"))?;
        repo::create(&result.repository, &common, &repo::Identity::default())?;
        let mut inspect = command(&result.git, &result.root);
        inspect.env("GIT_DIR", &result.repository).args([
            "show",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--binary",
            "--format=medium",
            &result.commit,
        ]);
        result.diff = said(&mut inspect, review::MAX_INPUT)?;
        result.subject = said(
            command(&result.git, &result.root)
                .env("GIT_DIR", &result.repository)
                .args(["show", "-s", "--format=%s", &result.commit]),
            8192,
        )?
        .trim_end()
        .to_string();
        let paths = said(
            command(&result.git, &result.root)
                .env("GIT_DIR", &result.repository)
                .args([
                    "diff-tree",
                    "--root",
                    "-m",
                    "--no-commit-id",
                    "--name-only",
                    "-r",
                    "-z",
                    &result.commit,
                ]),
            review::MAX_INPUT,
        )?;
        let mut selected: BTreeSet<String> = sparse.iter().cloned().collect();
        for path in paths.split('\0').filter(|s| !s.is_empty()) {
            if let Some((first, _)) = path.split_once('/') {
                selected.insert(first.to_string());
            }
        }
        // Repository configuration and instruction routing remain visible.
        selected.insert(".cargo".into());
        result.sparse = selected.into_iter().collect();
        if result.sparse.len() > MAX_PATHS {
            return Err("too many sparse review paths".into());
        }
        repo::add_worktree(
            &result.repository,
            &repo::Worktree {
                id: "review".into(),
                checkout: result.checkout.clone(),
                branch: "review".into(),
                sparse: Some(result.sparse.clone()),
            },
        )?;
        let task = repo::Task::Checkout {
            git: result.git.clone(),
            repository: result.repository.clone(),
            id: "review".into(),
            checkout: result.checkout.clone(),
            branch: "review".into(),
            base: result.commit.clone(),
        };
        jail::maintain(
            &result.programs,
            &result.maintenance(),
            &result.root.join("specs"),
            &task,
            GIT_TIME,
        )?;
        Ok(result)
    }

    fn maintenance(&self) -> jail::Policy {
        jail::Policy {
            home: self.root.join("home"),
            checkouts: vec![self.checkout.clone()],
            repositories: vec![self.repository.clone()],
            objects: vec![self.objects.clone()],
            directory: Some(self.checkout.clone()),
            ..jail::Policy::default()
        }
    }

    pub fn policy(&self) -> jail::Policy {
        jail::Policy {
            home: self.root.join("home"),
            read: vec![
                self.checkout.clone(),
                self.repository.clone(),
                self.objects.clone(),
            ],
            worktrees: vec![self.scratch.clone()],
            directory: Some(self.checkout.clone()),
            ..jail::Policy::default()
        }
    }

    pub fn expand(&mut self, paths: &[String]) -> Result<String, String> {
        let selected: BTreeSet<String> = self.sparse.iter().chain(paths).cloned().collect();
        if selected.len() > MAX_PATHS {
            return Err("too many sparse review paths".into());
        }
        let selected: Vec<String> = selected.into_iter().collect();
        repo::cone(Some(&selected))?;
        let task = repo::Task::Sparse {
            git: self.git.clone(),
            repository: self.repository.clone(),
            id: "review".into(),
            checkout: self.checkout.clone(),
            base: self.commit.clone(),
            paths: selected.clone(),
        };
        jail::maintain(
            &self.programs,
            &self.maintenance(),
            &self.root.join("specs"),
            &task,
            GIT_TIME,
        )?;
        self.sparse = selected;
        Ok(format!(
            "Sparse checkout at {} now includes {}",
            self.commit,
            self.sparse.join(", ")
        ))
    }

    pub fn call(&self, name: &str, arguments: &str) -> Result<String, String> {
        let tools::Args::Host { mut call, .. } =
            tools::parse_in(tools::Kit::Review, name, arguments)?
        else {
            return Err("this tool is unavailable in the review profile".into());
        };
        if let host::Call::Shell {
            command,
            timeout_ms,
            workdir,
        } = call
        {
            call = host::Call::ReviewShell {
                command,
                timeout_ms: Some(timeout_ms.unwrap_or(120_000).min(600_000)),
                workdir,
                target_dir: self
                    .scratch
                    .join("target")
                    .to_str()
                    .ok_or("scratch path is not UTF-8")?
                    .to_string(),
            };
        }
        if matches!(call, host::Call::Background { .. }) {
            return Err("background processes are unavailable in the review profile".into());
        }
        let mut client = jail::launch(&self.programs, &self.policy(), &self.root.join("specs"))?;
        let id = client.call(call)?;
        let deadline = Instant::now()
            .checked_add(TOOL_TIME)
            .ok_or("tool deadline overflow")?;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("the review tool exceeded its deadline".into());
            }
            if let Some(reply) = client.next_reply(left.min(Duration::from_secs(1))) {
                match reply? {
                    host::Up::Done { id: got, outcome } if got == id => {
                        let answer = outcome?.text;
                        if answer.len() > MAX_TOOL_REPLY {
                            return Err("review tool output exceeds its bound".into());
                        }
                        return Ok(answer);
                    }
                    host::Up::Output { .. } => {}
                    _ => return Err("unexpected reply from the review tool host".into()),
                }
            }
        }
    }

    pub fn cleanup(&mut self) -> Result<(), String> {
        if self.cleaned {
            return Ok(());
        }
        let parent = self.root.parent().ok_or("review directory has no parent")?;
        let serial = lock(&parent.join("sweep.lock"))?;
        serial.lock().map_err(|e| e.to_string())?;
        workspace::remove_tree(&self.root)
            .map_err(|e| format!("cleaning review workspace {}: {e}", self.root.display()))?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if let Err(why) = self.cleanup() {
            eprintln!("td-agent review: {why}");
        }
    }
}
