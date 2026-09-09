//! Git-only forced-command endpoint for individually reserved VM branches.
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, DirBuilder, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{symlink, DirBuilderExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "../vm_git_registry.rs"]
mod registry;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const LIMIT: u64 = 1024 * 1024;
const MAX_BRANCHES: usize = 4096;
static SERIAL: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, PartialEq, Eq)]
struct Policy {
    repository: PathBuf,
    git: PathBuf,
    path: String,
    branches: BTreeMap<String, String>,
    keys: BTreeMap<String, String>,
    dispatcher: PathBuf,
}

fn bounded(mut reader: impl Read) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.by_ref().take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err("input exceeds one MiB".into());
    }
    Ok(bytes)
}

fn instance_valid(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn branch_valid(branch: &str) -> bool {
    !branch.is_empty()
        && branch.len() <= 200
        && branch != "main"
        && branch != "HEAD"
        && !branch.starts_with("refs/")
        && !branch.contains("..")
        && branch.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with(['.', '-'])
                && !part.ends_with('.')
                && !part.ends_with(".lock")
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
}

fn absolute(value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    if !path.is_absolute()
        || value.chars().any(char::is_control)
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(
            "policy paths must be absolute without control characters or dot components".into(),
        );
    }
    Ok(path)
}

impl Policy {
    fn parse(text: &str) -> Result<Self> {
        let mut lines = text.lines();
        if lines.next() != Some("TDVM-GIT-2") || !text.ends_with('\n') {
            return Err("invalid policy header or missing final newline".into());
        }
        let mut fields = BTreeMap::new();
        let mut branches: BTreeMap<String, String> = BTreeMap::new();
        let mut keys = BTreeMap::new();
        for line in lines {
            let (key, value) = line.split_once('=').ok_or("invalid policy field")?;
            if key == "branch" {
                let (id, branch) = value.split_once(' ').ok_or("invalid branch reservation")?;
                if !instance_valid(id) || !branch_valid(branch) || branches.len() >= MAX_BRANCHES {
                    return Err("invalid branch reservation".into());
                }
                if branches.keys().any(|old| {
                    old == branch
                        || old
                            .strip_prefix(branch)
                            .is_some_and(|suffix| suffix.starts_with('/'))
                        || branch
                            .strip_prefix(old)
                            .is_some_and(|suffix| suffix.starts_with('/'))
                }) {
                    return Err("duplicate or overlapping branch reservation".into());
                }
                branches.insert(branch.into(), id.into());
            } else if key == "key" {
                let (id, encoded) = value.split_once(' ').ok_or("invalid instance key")?;
                registry::key_valid(encoded)?;
                if !instance_valid(id)
                    || keys.len() >= MAX_BRANCHES
                    || keys.contains_key(id)
                    || keys.values().any(|key| key == encoded)
                {
                    return Err("duplicate or invalid instance key".into());
                }
                keys.insert(id.to_string(), encoded.to_string());
            } else if !["repository", "git", "path", "dispatcher"].contains(&key)
                || fields.insert(key, value).is_some()
            {
                return Err("unknown or duplicate policy field".into());
            }
        }
        let repository = absolute(fields.get("repository").ok_or("missing repository")?)?;
        let git = absolute(fields.get("git").ok_or("missing git")?)?;
        let path = *fields.get("path").ok_or("missing path")?;
        for entry in path.split(':') {
            absolute(entry)?;
        }
        let owners: BTreeSet<_> = branches.values().collect();
        if owners.iter().any(|id| !keys.contains_key(*id))
            || keys.keys().any(|id| !owners.contains(id))
        {
            return Err("every branch needs a registered key and every key needs a branch".into());
        }
        let dispatcher = absolute(fields.get("dispatcher").ok_or("missing dispatcher")?)?;
        Ok(Self {
            repository,
            git,
            path: path.into(),
            branches,
            keys,
            dispatcher,
        })
    }

    fn private_parent(path: &Path) -> Result<()> {
        absolute(path.to_str().ok_or("policy path is not UTF-8")?)?;
        if path.file_name() == Some(std::ffi::OsStr::new(".td-vm-git.lock")) {
            return Err("policy path collides with the registry lock".into());
        }
        let uid = fs::metadata("/proc/self")?.uid();
        let parent = path.parent().ok_or("policy has no parent")?;
        let meta = fs::symlink_metadata(parent)?;
        if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
            return Err("policy parent must be a private, caller-owned directory".into());
        }
        for ancestor in parent.ancestors().skip(1) {
            let meta = fs::symlink_metadata(ancestor)?;
            if !meta.is_dir()
                || (meta.uid() != 0 && meta.uid() != uid)
                || (meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0)
            {
                return Err("untrusted policy ancestor".into());
            }
        }
        Ok(())
    }

    fn load(path: &Path) -> Result<Self> {
        Self::private_parent(path)?;
        let meta = fs::symlink_metadata(path)?;
        if !meta.is_file()
            || meta.uid() != fs::metadata("/proc/self")?.uid()
            || meta.mode() & 0o077 != 0
        {
            return Err("policy must be a private, caller-owned regular file".into());
        }
        Self::parse(&String::from_utf8(bounded(File::open(path)?)?)?)
    }

    fn authorized(&self, instance: &str, key: &str) -> bool {
        self.keys
            .get(instance)
            .is_some_and(|registered| registered == key)
            && self.branches.values().any(|owner| owner == instance)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.git);
        command
            .current_dir(&self.repository)
            .env_clear()
            .env("PATH", &self.path)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .arg("--no-replace-objects")
            .arg("--git-dir")
            .arg(&self.repository);
        command
    }

    fn query(&self, args: &[&str]) -> Result<String> {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let output = bounded(child.stdout.take().ok_or("missing git stdout")?);
        if output.is_err() {
            let _ = child.kill();
        }
        let status = child.wait()?;
        let output = output?;
        if !status.success() {
            return Err("Git repository query failed".into());
        }
        Ok(String::from_utf8(output)?.trim_end_matches('\n').into())
    }
}

fn operation(original: &str, repository: &Path) -> Result<&'static str> {
    let path = repository.to_str().ok_or("repository path is not UTF-8")?;
    let quoted = format!("'{}'", path.replace('\'', "'\\''"));
    for (wire, subcommand) in [
        ("git-upload-pack", "upload-pack"),
        ("git-receive-pack", "receive-pack"),
    ] {
        if original == format!("{wire} {quoted}") {
            return Ok(subcommand);
        }
    }
    Err("only Git upload/receive for the configured repository is allowed".into())
}

struct Session(PathBuf);
impl Session {
    fn create(parent: &Path) -> Result<Self> {
        for _ in 0..100 {
            let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("session-{}-{serial}", std::process::id()));
            match DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err("cannot allocate private Git session directory".into())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn install_hooks(session: &Session, original: &Path) -> Result<()> {
    if original.exists() {
        for (index, entry) in fs::read_dir(original)?.enumerate() {
            if index >= 256 {
                return Err("too many repository hook entries".into());
            }
            let entry = entry?;
            if entry.file_name() != "pre-receive" {
                symlink(env::current_exe()?, session.0.join(entry.file_name()))?;
            }
        }
    }
    symlink(env::current_exe()?, session.0.join("pre-receive"))?;
    Ok(())
}

fn serve(path: &Path, instance: &str, key: &str, original: &str) -> Result<bool> {
    let path = absolute(path.to_str().ok_or("policy path is not UTF-8")?)?;
    let policy = Policy::load(&path)?;
    if !policy.authorized(instance, key) {
        return Err("instance key is not enrolled with a branch reservation".into());
    }
    let operation = operation(original, &policy.repository)?;
    if policy.query(&["rev-parse", "--is-bare-repository"])? != "true" {
        return Err("VM origin must be a bare repository".into());
    }
    let mut command = policy.command();
    let mut session = None;
    if operation == "receive-pack" {
        if policy
            .query(&["config", "--null", "--list"])?
            .split('\0')
            .any(|entry| {
                entry
                    .strip_prefix("receive.procreceiverefs\n")
                    .is_some_and(|value| !value.is_empty())
            })
        {
            return Err("proc-receive ref rewriting is unsupported for VM pushes".into());
        }
        if policy
            .query(&["config", "--null", "--list"])?
            .split('\0')
            .any(|entry| entry.starts_with("hook."))
        {
            return Err("configured hook chains are unsupported for VM pushes".into());
        }
        let object_format = policy.query(&["rev-parse", "--show-object-format"])?;
        if !["sha1", "sha256"].contains(&object_format.as_str()) {
            return Err("unsupported object format".into());
        }
        let hooks = policy.query(&["rev-parse", "--git-path", "hooks"])?;
        let owned = Session::create(path.parent().ok_or("policy has no parent")?)?;
        install_hooks(&owned, &policy.repository.join(&hooks))?;
        command
            .arg("-c")
            .arg(format!("core.hooksPath={}", owned.0.display()))
            .env("TD_VM_GIT_POLICY", &path)
            .env("TD_VM_GIT_INSTANCE", instance)
            .env("TD_VM_GIT_KEY", key)
            .env("TD_VM_GIT_REPOSITORY", &policy.repository)
            .env("TD_VM_GIT_EXECUTABLE", &policy.git)
            .env("TD_VM_GIT_PATH", &policy.path)
            .env("TD_VM_GIT_FORMAT", object_format)
            .env("TD_VM_GIT_HOOKS", hooks);
        session = Some(owned);
    }
    if env::var("GIT_PROTOCOL").as_deref() == Ok("version=2") {
        command.env("GIT_PROTOCOL", "version=2");
    }
    let status = command.arg(operation).arg(&policy.repository).status()?;
    drop(session);
    Ok(status.success())
}

fn validate_updates<'a>(
    bytes: &'a [u8],
    policy: &Policy,
    instance: &str,
    oid_len: usize,
) -> Result<BTreeSet<&'a str>> {
    let text = std::str::from_utf8(bytes)?;
    if text.is_empty() || !text.ends_with('\n') {
        return Err("empty or incomplete receive transaction".into());
    }
    let mut seen = BTreeSet::new();
    for line in text.split_terminator('\n') {
        let mut fields = line.split(' ');
        let old = fields.next().ok_or("missing old object")?;
        let new = fields.next().ok_or("missing new object")?;
        let name = fields.next().ok_or("missing ref")?;
        if fields.next().is_some()
            || [old, new]
                .iter()
                .any(|oid| oid.len() != oid_len || !oid.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err("malformed receive transaction".into());
        }
        let branch = name
            .strip_prefix("refs/heads/")
            .ok_or("only reserved branches may be pushed")?;
        if policy.branches.get(branch).map(String::as_str) != Some(instance)
            || new.bytes().all(|b| b == b'0')
            || !seen.insert(name)
            || seen.len() > MAX_BRANCHES
        {
            return Err(
                "push is not an update to a unique branch reserved for this instance".into(),
            );
        }
    }
    Ok(seen)
}

fn pre_receive() -> Result<bool> {
    let policy = Policy::load(Path::new(&env::var("TD_VM_GIT_POLICY")?))?;
    let instance = env::var("TD_VM_GIT_INSTANCE")?;
    if !policy.authorized(&instance, &env::var("TD_VM_GIT_KEY")?)
        || policy.repository != Path::new(&env::var("TD_VM_GIT_REPOSITORY")?)
        || policy.git != Path::new(&env::var("TD_VM_GIT_EXECUTABLE")?)
        || policy.path != env::var("TD_VM_GIT_PATH")?
    {
        return Err("Git session policy changed or instance was revoked".into());
    }
    let oid_len = match env::var("TD_VM_GIT_FORMAT")?.as_str() {
        "sha1" => 40,
        "sha256" => 64,
        _ => return Err("invalid object format".into()),
    };
    let bytes = bounded(io::stdin().lock())?;
    let refs = validate_updates(&bytes, &policy, &instance, oid_len)?;
    for name in refs {
        // receive-pack dereferences symbolic branch refs; exact name checks
        // alone could authorize an alias that actually updates main.
        let status = policy
            .command()
            .args(["symbolic-ref", "--quiet", name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()?;
        match status.code() {
            Some(1) => {}
            Some(0) => return Err("reserved branch is a symbolic ref".into()),
            _ => return Err("cannot establish reserved branch ref type".into()),
        }
    }
    let mut child = hook_command("pre-receive", &[])?
        .stdin(Stdio::piped())
        .spawn()?;
    let result = child
        .stdin
        .take()
        .ok_or("missing hook stdin")?
        .write_all(&bytes);
    let status = child.wait()?;
    if let Err(error) = result {
        if error.kind() != io::ErrorKind::BrokenPipe {
            return Err(error.into());
        }
    }
    Ok(status.success())
}

fn hook_command(name: &str, args: &[std::ffi::OsString]) -> Result<Command> {
    // Git retains its shell fallback, argv[0], quarantine and hook exit rules.
    let mut command = Command::new(env::var("TD_VM_GIT_EXECUTABLE")?);
    command
        .current_dir(env::var("TD_VM_GIT_REPOSITORY")?)
        .arg("-c")
        .arg(format!("core.hooksPath={}", env::var("TD_VM_GIT_HOOKS")?))
        .args([
            "hook",
            "run",
            "--ignore-missing",
            "--to-stdin=/proc/self/fd/0",
            name,
            "--",
        ])
        .args(args);
    Ok(command)
}

fn run() -> Result<bool> {
    let mut args = env::args_os();
    let executable = args.next().ok_or("missing argv[0]")?;
    let remaining: Vec<_> = args.collect();
    // Forced-command argv always selects serve before considering hook mode;
    // client environment cannot turn the SSH entry point into a hook launcher.
    if remaining.first().is_some_and(|verb| verb == "serve") {
        let [_, path, instance, key] = remaining.as_slice() else {
            return Err("usage: td-vm-git serve POLICY INSTANCE KEY_BASE64".into());
        };
        return serve(
            Path::new(path),
            instance.to_str().ok_or("invalid instance")?,
            key.to_str().ok_or("invalid key identity")?,
            &env::var("SSH_ORIGINAL_COMMAND")?,
        );
    }
    if let Some(result) = registry::cli(&remaining) {
        return result;
    }
    let name = Path::new(&executable)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("invalid hook name")?;
    if name == "pre-receive" {
        return pre_receive();
    }
    if env::var_os("TD_VM_GIT_HOOKS").is_some()
        && name != "td-vm-git"
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Ok(hook_command(name, &remaining)?.status()?.success());
    }
    Err(
        "usage: td-vm-git check|init|enroll|reserve|revoke|authorized-keys|serve (see td-review/VM.md)"
            .into(),
    )
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("td-vm-git: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    const KEY_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB";
    const KEY_B: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIAICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgIC";
    const A: &str = "0123456789abcdef0123456789abcdef";
    const B: &str = "1123456789abcdef0123456789abcdef";
    fn policy() -> String {
        format!("TDVM-GIT-2\ndispatcher=/bin/td-vm-git\nkey={A} {KEY_A}\nkey={B} {KEY_B}\nrepository=/srv/git/td.git\ngit=/bin/git\npath=/bin:/usr/bin\nbranch={A} vm-a\nbranch={B} vm-b\n")
    }
    #[test]
    fn strict_policy_and_exact_ownership() {
        let parsed = Policy::parse(&policy()).unwrap();
        assert!(parsed.authorized(A, KEY_A));
        for suffix in [
            "unknown=x\n",
            "git=/other\n",
            &format!("branch={B} vm-a\n"),
            &format!("branch={B} vm-a/child\n"),
            &format!("branch={A} main\n"),
        ] {
            assert!(Policy::parse(&(policy() + suffix)).is_err(), "{suffix}");
        }
        for branch in [
            "",
            "main",
            "HEAD",
            "refs/heads/a",
            "a..b",
            "-a",
            "a/-b",
            "a.lock",
            "a/.b",
            "a//b",
            "a/b.",
            "a b",
        ] {
            assert!(!branch_valid(branch), "{branch}");
        }
        for branch in ["fix-clipboard", "vm/topic_2", "ui-rolling"] {
            assert!(branch_valid(branch));
        }
        assert!(Policy::parse(&policy().replace("path=/bin:/usr/bin", "path=/bin:")).is_err());
    }
    #[test]
    fn command_is_exact_and_never_shell_evaluated() {
        let repo = Path::new("/srv/git/td.git");
        assert_eq!(
            operation("git-upload-pack '/srv/git/td.git'", repo).unwrap(),
            "upload-pack"
        );
        assert_eq!(
            operation("git-receive-pack '/srv/git/td.git'", repo).unwrap(),
            "receive-pack"
        );
        for command in [
            "",
            "sh",
            "git-upload-pack '/srv/git/td.git'; id",
            "git-upload-pack '/srv/git/other.git'",
            "git-upload-pack /srv/git/td.git",
            "git-upload-pack '/srv/git/td.git'\n",
        ] {
            assert!(operation(command, repo).is_err());
        }
        assert!(operation("git-upload-pack '/a'\\''b'", Path::new("/a'b")).is_ok());
    }
    #[test]
    fn whole_receive_transaction_is_authorized() {
        let parsed = Policy::parse(&policy()).unwrap();
        let oid = "1".repeat(40);
        let zero = "0".repeat(40);
        let valid = format!("{zero} {oid} refs/heads/vm-a\n");
        assert!(validate_updates(valid.as_bytes(), &parsed, A, 40).is_ok());
        for invalid in [
            String::new(),
            valid.trim_end().into(),
            valid.replace("vm-a", "main"),
            valid.replace("vm-a", "vm-b"),
            valid.replace("heads/vm-a", "tags/vm-a"),
            valid.replace("vm-a", "vm-a/child"),
            valid.replace(&oid, &zero),
            valid.repeat(2),
            valid.clone() + &valid.replace("vm-a", "vm-b"),
            valid.replace(&oid, "garbage"),
        ] {
            assert!(
                validate_updates(invalid.as_bytes(), &parsed, A, 40).is_err(),
                "{invalid}"
            );
        }
        let sha256 = valid
            .replace(&oid, &"1".repeat(64))
            .replace(&zero, &"0".repeat(64));
        assert!(validate_updates(sha256.as_bytes(), &parsed, A, 64).is_ok());
    }
    #[test]
    fn inputs_are_bounded() {
        assert!(bounded(io::repeat(0)).is_err());
        assert_eq!(bounded(&b"abc"[..]).unwrap(), b"abc");
    }
}
