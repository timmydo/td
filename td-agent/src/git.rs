//! The git worker outside any jail (DESIGN.md §7, §9): admitted remotes,
//! ref names, and the store, one bare repository per remote that no jail
//! can write. Every git it runs here is fixed in shape (`Worker::command`):
//! a cleared environment, `GIT_DIR` named, no configuration of the
//! human's but the identity and credential-helper lines td-agent copies,
//! and only the `https` and `ssh` transports. The remote's URL is always
//! td-agent's record of it, passed after `--end-of-options`; the store
//! holds no remote configuration at all.

use std::ffi::OsString;
use std::fs::DirBuilder;
use std::io::Read;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The longest remote, base or branch text (DESIGN.md §15).
pub const MAX_TEXT: usize = 2048;
/// The largest file read from a base commit: project instructions and
/// `.td-agent/rules`.
pub const MAX_FILE: u64 = 64 * 1024;
/// The variables an outside git keeps from td-agent's environment; the
/// rest is cleared (DESIGN.md §9). `XDG_CONFIG_HOME` only locates the
/// human's configuration for the copy: every other git names
/// `GIT_CONFIG_GLOBAL`, which git then reads alone.
const KEPT: &[&str] = &[
    "PATH",
    "LANG",
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "SSH_AUTH_SOCK",
];
/// The human's global configuration lines an outside git keeps: the
/// identity, and what a credential helper reads.
const COPIED: &str = r"^(user\.(name|email)|credential\.(.+\.)?(helper|username|usehttppath))$";
/// How long a fetch may take, a first clone of a large repository
/// included; a stalled transfer ends sooner (`STALL`).
const FETCH_TIME: Duration = Duration::from_secs(60 * 60);
/// How long any other git may take: it reads or writes local files.
const LOCAL_TIME: Duration = Duration::from_secs(120);
/// The settings that end a stalled transfer: https below a kilobyte a
/// second for a minute, ssh that cannot connect in half a minute or
/// whose server stops answering for a minute.
const STALL: &[&str] = &[
    "http.lowSpeedLimit=1000",
    "http.lowSpeedTime=60",
    "core.sshCommand=ssh -o BatchMode=yes -o ConnectTimeout=30 -o ServerAliveInterval=15 -o ServerAliveCountMax=4",
];

/// A transport td-agent admits: never `file`, a local path, `ext`,
/// `git` or plain `http`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Transport {
    Https,
    Ssh,
}

/// A git remote as td-agent records it, parsed and checked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Remote {
    pub transport: Transport,
    /// The ssh user, as written; https takes none.
    pub user: Option<String>,
    /// Lower case.
    pub host: String,
    /// None for the transport's default port.
    pub port: Option<u16>,
    /// The path's segments as written, a trailing `.git` kept.
    pub segments: Vec<String>,
    /// Written scp-like with a path relative to the login's home, which
    /// an `ssh://` URL cannot spell; git is given that form back.
    pub relative: bool,
    /// A test's local repository, fetched over the file transport that
    /// only tests admit.
    #[cfg(test)]
    pub local: Option<PathBuf>,
}

impl Remote {
    /// Parses `text`: `https://host[:port]/path`, `ssh://[user@]host[:port]/path`
    /// or the scp-like `[user@]host:path`, refusing every other
    /// transport and anything a URL could smuggle (credentials, a query,
    /// an option-like host, `..`).
    pub fn parse(text: &str) -> Result<Self, String> {
        let refuse = |why: &str| format!("the remote {text:?} {why}");
        if text.is_empty() || text.len() > MAX_TEXT {
            return Err(refuse(&format!("is not 1 to {MAX_TEXT} bytes")));
        }
        if text.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(refuse("holds a space or a control character"));
        }
        let (transport, rest, scp) = if let Some(rest) = strip_scheme(text, "https://") {
            (Transport::Https, rest, false)
        } else if let Some(rest) = strip_scheme(text, "ssh://") {
            (Transport::Ssh, rest, false)
        } else if text.contains("://") || text.starts_with("ext::") {
            return Err(refuse(
                "is not https or ssh: td-agent admits no other transport",
            ));
        } else {
            // scp-like: a colon before any slash; a local path has none.
            match text.split_once(':') {
                Some((before, _)) if !before.is_empty() && !before.contains('/') => {
                    (Transport::Ssh, text, true)
                }
                _ => {
                    return Err(refuse(
                        "is a local path: td-agent admits only https and ssh remotes",
                    ))
                }
            }
        };
        let (authority, path) = if scp {
            rest.split_once(':').ok_or_else(|| refuse("has no path"))?
        } else {
            rest.split_once('/').ok_or_else(|| refuse("has no path"))?
        };
        let (user, hostport) = match authority.rsplit_once('@') {
            Some((user, hostport)) => (Some(user), hostport),
            None => (None, authority),
        };
        if transport == Transport::Https && user.is_some() {
            return Err(refuse(
                "carries credentials; the credential helper supplies them",
            ));
        }
        if let Some(user) = user {
            if !word(user) {
                return Err(refuse(
                    "has a user that is not letters, digits, `.`, `_` or `-`",
                ));
            }
        }
        if hostport.starts_with('[') {
            return Err(refuse(
                "names an IPv6 address, which td-agent does not admit",
            ));
        }
        let (host, port) = match hostport.split_once(':') {
            Some(_) if scp => return Err(refuse("names a port in the scp-like form")),
            Some((host, port)) => {
                let port: u16 = Some(port)
                    .filter(|p| !p.is_empty() && !p.starts_with('0'))
                    .filter(|p| p.bytes().all(|b| b.is_ascii_digit()))
                    .and_then(|p| p.parse().ok())
                    .ok_or_else(|| refuse("has a port that is not 1 to 65535"))?;
                (host, Some(port))
            }
            None => (hostport, None),
        };
        let host = host.to_ascii_lowercase();
        let host_ok = !host.is_empty()
            && !host.starts_with(['-', '.'])
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
        if !host_ok {
            return Err(refuse(
                "has a host that is not letters, digits, `.` and `-`, starting with neither",
            ));
        }
        let port = match (transport, port) {
            (Transport::Https, Some(443)) | (Transport::Ssh, Some(22)) => None,
            (_, port) => port,
        };
        let relative = scp && !path.starts_with('/');
        let segments = segments(path).map_err(|why| refuse(&why))?;
        Ok(Self {
            transport,
            user: user.map(str::to_string),
            host,
            port,
            segments,
            relative,
            #[cfg(test)]
            local: None,
        })
    }

    /// The URL git is given: always this record, never a file a jail can
    /// write.
    pub fn url(&self) -> String {
        #[cfg(test)]
        if let Some(local) = &self.local {
            return format!("file://{}", local.display());
        }
        let (scheme, user) = match self.transport {
            Transport::Https => ("https", String::new()),
            Transport::Ssh => (
                "ssh",
                self.user
                    .as_ref()
                    .map_or_else(String::new, |user| format!("{user}@")),
            ),
        };
        if self.relative {
            return format!("{user}{}:{}", self.host, self.segments.join("/"));
        }
        let port = self
            .port
            .map_or_else(String::new, |port| format!(":{port}"));
        format!(
            "{scheme}://{user}{}{port}/{}",
            self.host,
            self.segments.join("/")
        )
    }

    /// The path compared for admission: its segments, a final `.git` off.
    fn key(&self) -> Vec<&str> {
        let mut key: Vec<&str> = self.segments.iter().map(String::as_str).collect();
        if let Some(last) = key.last_mut() {
            *last = last.strip_suffix(".git").unwrap_or(last);
        }
        key
    }

    /// The store's name for this remote: its host and path made one
    /// file name, then a digest of the whole record, so that two remotes
    /// never share a store.
    pub fn store_name(&self) -> String {
        let mut name: String = std::iter::once(self.host.as_str())
            .chain(self.key())
            .collect::<Vec<_>>()
            .join("-")
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        name.truncate(96);
        format!("{name}-{:016x}", fnv(self.identity().as_bytes()))
    }

    /// Everything that names the repository fetched, a final `.git`
    /// included, since a plain server may hold both: the store records
    /// it, so a digest that collided would be refused, not shared.
    fn identity(&self) -> String {
        format!(
            "{:?} {} {} {} {} {}",
            self.transport,
            self.user.as_deref().unwrap_or(""),
            self.host,
            self.port.unwrap_or(0),
            self.relative,
            self.segments.join("/")
        )
    }
}

/// `text` past `scheme`, the scheme compared without case.
fn strip_scheme<'a>(text: &'a str, scheme: &str) -> Option<&'a str> {
    let head = text.get(..scheme.len())?;
    head.eq_ignore_ascii_case(scheme)
        .then(|| text.get(scheme.len()..))
        .flatten()
}

/// Letters, digits, `.`, `_` and `-`, not starting with `-`.
fn word(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with('-')
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A remote path's segments: no empty one but a trailing slash, no `.`
/// or `..`, none starting with `-`, and only URL-safe characters.
fn segments(path: &str) -> Result<Vec<String>, String> {
    let path = path.strip_prefix('/').unwrap_or(path);
    let path = path.strip_suffix('/').unwrap_or(path);
    if path.is_empty() {
        return Err("has no path".into());
    }
    path.split('/')
        .map(|segment| {
            let ok = !segment.is_empty()
                && segment != "."
                && segment != ".."
                && !segment.starts_with('-')
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '~' | '+' | '@'));
            ok.then(|| segment.to_string()).ok_or_else(|| {
                format!("has a path segment {segment:?} that is empty, `.`, `..`, option-like or not URL-safe")
            })
        })
        .collect()
}

/// FNV-1a, 64 bits: a name's digest, not a secret's.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// One entry of `remotes` (DESIGN.md §7, §15): a remote's URL, admitting
/// that remote, or `host/prefix`, admitting every https or ssh remote on
/// the host whose path starts with the prefix's whole segments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Admission {
    Exact(Remote),
    Prefix { host: String, segments: Vec<String> },
}

impl Admission {
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_TEXT {
            return Err(format!("a remote prefix is at most {MAX_TEXT} bytes"));
        }
        let scheme = text.contains("://");
        let scp = !scheme
            && text
                .split_once(':')
                .is_some_and(|(before, _)| !before.contains('/'));
        if scheme || scp {
            return Remote::parse(text).map(Self::Exact);
        }
        let (host, path) = text.split_once('/').unwrap_or((text, ""));
        // A prefix is a remote's host and path without a transport.
        let probe = Remote::parse(&format!("https://{host}/x"))
            .map_err(|_| format!("the remote prefix {text:?} has no valid host"))?;
        let mut segments = if path.trim_matches('/').is_empty() {
            Vec::new()
        } else {
            segments(path).map_err(|why| format!("the remote prefix {text:?} {why}"))?
        };
        // Compared as a remote's path is, a final `.git` off.
        if let Some(last) = segments.last_mut() {
            if let Some(stem) = last.strip_suffix(".git").filter(|s| !s.is_empty()) {
                *last = stem.to_string();
            }
        }
        Ok(Self::Prefix {
            host: probe.host,
            segments,
        })
    }

    /// Whether this entry admits `remote`.
    pub fn admits(&self, remote: &Remote) -> bool {
        match self {
            Self::Exact(admitted) => {
                admitted.transport == remote.transport
                    && admitted.user == remote.user
                    && admitted.host == remote.host
                    && admitted.port == remote.port
                    && admitted.relative == remote.relative
                    && admitted.key() == remote.key()
            }
            Self::Prefix { host, segments } => {
                let key = remote.key();
                *host == remote.host
                    && segments.len() <= key.len()
                    && segments.iter().zip(&key).all(|(a, b)| a == b)
            }
        }
    }
}

/// `name` checked as a branch name git accepts (`git check-ref-format
/// --branch`), and never one an argument could misread: no leading `-`,
/// `+` or `:`.
pub fn branch_name(name: &str) -> Result<&str, String> {
    let refuse = || format!("{name:?} is not a branch name td-agent admits");
    let bad_char = |c: char| {
        c.is_ascii_control()
            || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\' | '\u{7f}')
    };
    let ok = !name.is_empty()
        && name.len() <= MAX_TEXT
        && !name.starts_with(['-', '+', ':', '/'])
        && !name.ends_with(['/', '.'])
        && !name.ends_with(".lock")
        && name != "@"
        && name != "HEAD"
        && !name.contains("..")
        && !name.contains("@{")
        && !name.contains("//")
        && !name.chars().any(bad_char)
        && name
            .split('/')
            .all(|part| !part.starts_with('.') && !part.ends_with(".lock"));
    ok.then_some(name).ok_or_else(refuse)
}

/// The git worker's outside half: the fixed shape of every git it runs
/// on a repository no jail can write.
#[derive(Debug)]
pub struct Worker {
    /// The global configuration td-agent writes: the human's identity and
    /// credential helpers, nothing else.
    config: PathBuf,
    /// An empty directory td-agent owns, git's hooks path.
    hooks: PathBuf,
    /// What an outside git keeps of td-agent's environment.
    env: Vec<(String, OsString)>,
    /// Tests fetch from local repositories, which no product build does.
    #[cfg(test)]
    file: bool,
}

impl Worker {
    /// A worker whose private files live in `dir`, td-agent's own: the
    /// global configuration is written afresh from the human's global
    /// configuration, which `env`'s `HOME` locates, keeping only
    /// `user.name`, `user.email` and credential helpers.
    pub fn new(dir: &Path, env: &[(String, OsString)]) -> Result<Self, String> {
        // git runs in `dir`, so every path it is given must not depend on
        // td-agent's own directory.
        if !dir.is_absolute() {
            return Err(format!(
                "the git worker's {} is not absolute",
                dir.display()
            ));
        }
        let make = |path: &Path| {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .map_err(|e| format!("{}: {e}", path.display()))
        };
        make(dir)?;
        let hooks = dir.join("hooks");
        make(&hooks)?;
        let env: Vec<(String, OsString)> = env
            .iter()
            .filter(|(name, _)| KEPT.contains(&name.as_str()))
            .cloned()
            .collect();
        let config = dir.join("gitconfig");
        let worker = Self {
            config,
            hooks,
            env,
            #[cfg(test)]
            file: false,
        };
        worker.copy_identity()?;
        Ok(worker)
    }

    /// Writes the global configuration: the copied lines, each through
    /// git's own writer so nothing needs quoting by hand.
    fn copy_identity(&self) -> Result<(), String> {
        // Written beside it and then put in its place, so a worker never
        // reads a half-written one.
        let fresh = self.config.with_extension("new");
        let _ = std::fs::remove_file(&fresh);
        std::fs::write(&fresh, b"").map_err(|e| format!("{}: {e}", fresh.display()))?;
        let mut read = self.bare();
        read.env("GIT_CONFIG_NOSYSTEM", "1").args([
            "config",
            "--global",
            "--includes",
            "--null",
            "--get-regexp",
            COPIED,
        ]);
        let found = match run(&mut read, 1024 * 1024, LOCAL_TIME) {
            Ok(out) => out,
            // Exit 1 is git's "nothing matched".
            Err(Failure::Exit(1, _)) => Vec::new(),
            Err(e) => return Err(format!("reading the human's git identity: {e}")),
        };
        for entry in found.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            let entry = String::from_utf8_lossy(entry);
            // A key with no value is a boolean's true, which only
            // `useHttpPath` can be; an empty value, as for
            // `credential.helper`, resets the list and is kept.
            let (key, value) = match entry.split_once('\n') {
                Some(pair) => pair,
                None if entry.ends_with(".usehttppath") => (&*entry, "true"),
                None => continue,
            };
            let mut write = self.bare();
            write.args(["config", "--file"]).arg(&fresh).args([
                "--add",
                "--end-of-options",
                key,
                value,
            ]);
            run(&mut write, 4096, LOCAL_TIME)
                .map_err(|e| format!("writing {}: {e}", fresh.display()))?;
        }
        std::fs::rename(&fresh, &self.config).map_err(|e| format!("{}: {e}", self.config.display()))
    }

    /// git with nothing but the kept environment, in td-agent's own
    /// directory with discovery stopped there, so a git that needs no
    /// repository never finds one where the window was started.
    fn bare(&self) -> Command {
        let mut command = Command::new("git");
        command
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null());
        if let Some(dir) = self.hooks.parent() {
            command
                .current_dir(dir)
                .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap_or(dir));
        }
        command
    }

    /// git over `git_dir` in the fixed shape (DESIGN.md §9).
    fn command(&self, git_dir: &Path) -> Command {
        let mut command = self.bare();
        command
            .env("GIT_DIR", git_dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &self.config)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .arg("-c")
            .arg(format!("core.hooksPath={}", self.hooks.display()));
        for setting in [
            "core.fsmonitor=false",
            "core.attributesFile=/dev/null",
            // The store is borrowed through alternates: nothing prunes it
            // behind td-agent's back (DESIGN.md §9, Object lifetime).
            "gc.auto=0",
            "maintenance.auto=false",
            "protocol.allow=never",
            "protocol.https.allow=always",
            "protocol.ssh.allow=always",
            "submodule.recurse=false",
            "fetch.recurseSubmodules=false",
            "push.recurseSubmodules=no",
            "http.followRedirects=false",
            "credential.interactive=false",
        ]
        .iter()
        .chain(STALL)
        {
            command.arg("-c").arg(setting);
        }
        #[cfg(test)]
        if self.file {
            command.args(["-c", "protocol.file.allow=always"]);
        }
        command
    }

    /// Makes the store for `remote` under `root` when there is none, a
    /// bare repository with no remote configured, and returns its path.
    /// The remote's record is written first, whole, so a store is never
    /// one without it, and `init` then runs whenever the store lacks its
    /// `HEAD`, which repeats safely after a crash or beside a second call.
    pub fn store(&self, root: &Path, remote: &Remote) -> Result<PathBuf, String> {
        if !root.is_absolute() {
            return Err(format!("the store root {} is not absolute", root.display()));
        }
        let git_dir = root.join(format!("{}.git", remote.store_name()));
        let marker = git_dir.join("td-agent-remote");
        let identity = remote.identity();
        match std::fs::read_to_string(&marker) {
            // A store made for another remote whose name collided.
            Ok(recorded) if recorded != identity => {
                return Err(format!(
                    "the store {} was made for another remote",
                    git_dir.display()
                ))
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if git_dir.join("HEAD").exists() {
                    return Err(format!("the store {} records no remote", git_dir.display()));
                }
                DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&git_dir)
                    .map_err(|e| format!("{}: {e}", git_dir.display()))?;
                // Written whole under a name of this call's own, then
                // linked into place, which only one call can do.
                static CALLS: AtomicU64 = AtomicU64::new(0);
                let fresh = git_dir.join(format!(
                    "td-agent-remote.{}.{}",
                    std::process::id(),
                    CALLS.fetch_add(1, Ordering::Relaxed)
                ));
                let placed = std::fs::write(&fresh, identity.as_bytes())
                    .and_then(|()| std::fs::hard_link(&fresh, &marker));
                let _ = std::fs::remove_file(&fresh);
                match placed {
                    Ok(()) => {}
                    // A second call placed it first: it must agree.
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        let recorded = std::fs::read_to_string(&marker)
                            .map_err(|e| format!("{}: {e}", marker.display()))?;
                        if recorded != identity {
                            return Err(format!(
                                "the store {} was made for another remote",
                                git_dir.display()
                            ));
                        }
                    }
                    Err(e) => return Err(format!("{}: {e}", marker.display())),
                }
            }
            Err(e) => return Err(format!("{}: {e}", marker.display())),
        }
        if !git_dir.join("HEAD").is_file() {
            let mut init = self.bare();
            init.env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", &self.config)
                .args([
                    "-c",
                    "init.defaultBranch=main",
                    "init",
                    "--quiet",
                    "--bare",
                    "--",
                ])
                .arg(&git_dir);
            // A second call's `init` may hold git's lock; its store is ours.
            if let Err(e) = run(&mut init, 64 * 1024, LOCAL_TIME) {
                if !git_dir.join("HEAD").is_file() {
                    return Err(format!("making the store {}: {e}", git_dir.display()));
                }
            }
        }
        Ok(git_dir)
    }

    /// Fetches every branch of `remote` into its store, pruning the ones
    /// gone: the only place objects are downloaded.
    pub fn fetch(&self, store: &Path, remote: &Remote) -> Result<(), String> {
        let mut fetch = self.command(store);
        fetch
            .args([
                "fetch",
                "--quiet",
                "--prune",
                "--no-tags",
                "--no-write-fetch-head",
                "--no-auto-maintenance",
                "--end-of-options",
            ])
            .arg(remote.url())
            .arg("+refs/heads/*:refs/heads/*");
        run(&mut fetch, 64 * 1024, FETCH_TIME)
            .map(drop)
            .map_err(|e| format!("fetching {}: {e}", remote.url()))
    }

    /// The commit the store's branch `base` names.
    pub fn resolve(&self, store: &Path, base: &str) -> Result<String, String> {
        let base = branch_name(base)?;
        let mut parse = self.command(store);
        parse
            .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
            .arg(format!("refs/heads/{base}^{{commit}}"));
        let out = match run(&mut parse, 256, LOCAL_TIME) {
            Ok(out) => out,
            Err(Failure::Exit(1, _)) => return Err(format!("the remote has no branch {base:?}")),
            Err(e) => return Err(format!("resolving {base:?}: {e}")),
        };
        let id = String::from_utf8_lossy(&out).trim().to_string();
        object_id(&id)
            .then_some(id)
            .ok_or_else(|| format!("git named {base:?} oddly"))
    }

    /// The file at `path` in commit `id` of the store, none when the
    /// commit has no such regular file (a tree, a link and a submodule are
    /// none); refused past `MAX_FILE` bytes. Its entry is looked up once,
    /// the path literal, and the blob read by its id.
    pub fn read(&self, store: &Path, id: &str, path: &str) -> Result<Option<Vec<u8>>, String> {
        if !object_id(id) {
            return Err(format!("{id:?} is not a commit id"));
        }
        let mut list = self.command(store);
        list.env("GIT_LITERAL_PATHSPECS", "1").args([
            "ls-tree",
            "-z",
            "--long",
            "--full-tree",
            "--end-of-options",
            id,
            "--",
            path,
        ]);
        let listed = run(&mut list, 64 * 1024, LOCAL_TIME).map_err(|e| format!("{path}: {e}"))?;
        for entry in listed.split(|b| *b == 0) {
            let Some((meta, name)) = entry
                .iter()
                .position(|b| *b == b'\t')
                .and_then(|at| Some((entry.get(..at)?, entry.get(at + 1..)?)))
            else {
                continue;
            };
            if name != path.as_bytes() {
                continue;
            }
            let meta = String::from_utf8_lossy(meta);
            let fields: Vec<&str> = meta.split_whitespace().collect();
            let [mode, kind, blob, size] = fields.as_slice() else {
                return Err(format!("{path}: git listed it oddly"));
            };
            if *kind != "blob" || !matches!(*mode, "100644" | "100755") {
                return Ok(None);
            }
            let size: u64 = size
                .parse()
                .map_err(|_| format!("{path}: git gave no size"))?;
            if size > MAX_FILE {
                return Err(format!(
                    "{path} is {size} bytes, more than the {MAX_FILE} read"
                ));
            }
            if !object_id(blob) {
                return Err(format!("{path}: git listed it oddly"));
            }
            let mut read = self.command(store);
            read.args(["cat-file", "blob", "--end-of-options", blob]);
            return run(&mut read, MAX_FILE, LOCAL_TIME)
                .map(Some)
                .map_err(|e| format!("{path}: {e}"));
        }
        Ok(None)
    }

    /// The project instructions at the top of commit `id` (DESIGN.md
    /// §13): `AGENTS.md`, else `CLAUDE.md`, with the name it came from.
    pub fn instructions(
        &self,
        store: &Path,
        id: &str,
    ) -> Result<Option<(&'static str, Vec<u8>)>, String> {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            if let Some(text) = self.read(store, id, name)? {
                return Ok(Some((name, text)));
            }
        }
        Ok(None)
    }
}

/// Why a git run failed.
#[derive(Debug)]
enum Failure {
    Start(std::io::Error),
    /// Its exit code, -1 when a signal ended it, and the start of what
    /// it said.
    Exit(i32, String),
    TooLong,
    TimedOut(Duration),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start(e) => write!(f, "git did not start: {e}"),
            Self::Exit(code, said) if said.is_empty() => write!(f, "git exited {code}"),
            Self::Exit(code, said) => write!(f, "git exited {code}: {said}"),
            Self::TooLong => write!(f, "git said more than was asked for"),
            Self::TimedOut(time) => write!(f, "git took more than {} seconds", time.as_secs()),
        }
    }
}

/// One of a run's pipes, read to its end.
enum Piece {
    Out(Vec<u8>),
    Err(Vec<u8>),
    /// One pipe reached its end.
    Closed,
}

/// Reads `from` to its end on a thread, telling what it read as it comes.
/// Standard output is read to one byte past `cap`, which is enough to
/// refuse it; standard error is read to its end and only its first `cap`
/// bytes told.
fn reader(mut from: impl Read + Send + 'static, tell: mpsc::Sender<Piece>, out: bool, cap: u64) {
    std::thread::spawn(move || {
        let mut buffer = vec![0u8; 8192];
        let mut told = 0u64;
        loop {
            if out && told > cap {
                break;
            }
            match from.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) if !out && told >= cap => {}
                Ok(n) => {
                    told = told.saturating_add(n as u64);
                    let bytes = buffer.get(..n).unwrap_or_default().to_vec();
                    let piece = if out {
                        Piece::Out(bytes)
                    } else {
                        Piece::Err(bytes)
                    };
                    if tell.send(piece).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = tell.send(Piece::Closed);
    });
}

/// How long the pipes may stay open once git has exited: a helper it
/// left behind can hold them.
const GRACE: Duration = Duration::from_secs(1);

/// Runs `command` for at most `time`, returning at most `limit` bytes of
/// its standard output. Its standard error is read to its end, so a git
/// that says much never dies on a closed pipe, and its start kept for the
/// reason. git's own exit decides: once it has exited, what its pipes
/// gave within `GRACE` is its answer. Past `time`, or past `limit`, git
/// is killed and the readers left to end with whatever it started.
fn run(command: &mut Command, limit: u64, time: Duration) -> Result<Vec<u8>, Failure> {
    let now = Instant::now();
    let deadline = now.checked_add(time).unwrap_or(now);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(Failure::Start)?;
    let (tell, heard) = mpsc::channel();
    let mut open = 0;
    if let Some(stdout) = child.stdout.take() {
        reader(stdout, tell.clone(), true, limit);
        open += 1;
    }
    if let Some(stderr) = child.stderr.take() {
        reader(stderr, tell.clone(), false, 4096);
        open += 1;
    }
    drop(tell);
    let stop = |child: &mut std::process::Child, why: Failure| {
        let _ = child.kill();
        let _ = child.wait();
        Err(why)
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut exited: Option<(std::process::ExitStatus, Instant)> = None;
    while open > 0 {
        let now = Instant::now();
        if exited.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => exited = Some((status, now.checked_add(GRACE).unwrap_or(now))),
                Ok(None) if now >= deadline => return stop(&mut child, Failure::TimedOut(time)),
                Ok(None) => {}
                Err(e) => return stop(&mut child, Failure::Start(e)),
            }
        }
        let until = exited.map_or(deadline, |(_, grace)| grace.min(deadline));
        if exited.is_some() && now >= until {
            break;
        }
        let wait = until
            .saturating_duration_since(now)
            .min(Duration::from_millis(20));
        match heard.recv_timeout(wait) {
            Ok(Piece::Out(bytes)) => {
                out.extend_from_slice(&bytes);
                if out.len() as u64 > limit {
                    return stop(&mut child, Failure::TooLong);
                }
            }
            // The start is kept for the reason; the rest is drained.
            Ok(Piece::Err(bytes)) => {
                let room = 4096usize.saturating_sub(err.len());
                err.extend_from_slice(bytes.get(..room.min(bytes.len())).unwrap_or_default());
            }
            Ok(Piece::Closed) => open -= 1,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => open = 0,
        }
    }
    let status = match exited {
        Some((status, _)) => status,
        None => loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Ok(None) => return stop(&mut child, Failure::TimedOut(time)),
                Err(e) => return stop(&mut child, Failure::Start(e)),
            }
        },
    };
    if status.success() {
        return Ok(out);
    }
    let said = String::from_utf8_lossy(&err);
    let said: String = crate::tools::visible(said.trim())
        .chars()
        .take(300)
        .collect();
    Err(Failure::Exit(status.code().unwrap_or(-1), said))
}

/// A full object id, as git names one: 40 or 64 hexadecimal digits.
fn object_id(id: &str) -> bool {
    (id.len() == 40 || id.len() == 64) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// What an outside git keeps of this process's environment.
pub fn kept_env() -> Vec<(String, OsString)> {
    KEPT.iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (name.to_string(), value)))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use crate::store::tests::Scratch;

    #[test]
    fn remotes_are_https_or_ssh_and_nothing_smuggled() {
        let https = Remote::parse("HTTPS://GitHub.com:443/timmydo/td.git/").unwrap();
        assert_eq!(https.url(), "https://github.com/timmydo/td.git");
        assert_eq!(https.transport, Transport::Https);
        // An scp-like path relative to the login's home stays relative;
        // an absolute one is the ssh URL's.
        let scp = Remote::parse("git@github.com:timmydo/td").unwrap();
        assert_eq!(scp.url(), "git@github.com:timmydo/td");
        let rooted = Remote::parse("git@example.org:/srv/td").unwrap();
        assert_eq!(rooted.url(), "ssh://git@example.org/srv/td");
        assert_ne!(
            scp.store_name(),
            Remote::parse("ssh://git@github.com/timmydo/td")
                .unwrap()
                .store_name()
        );
        let ssh = Remote::parse("ssh://git@example.org:2222/srv/repo").unwrap();
        assert_eq!(ssh.url(), "ssh://git@example.org:2222/srv/repo");
        for (text, why) in [
            ("http://github.com/a/b", "not https or ssh"),
            ("git://github.com/a/b", "not https or ssh"),
            ("file:///srv/a", "not https or ssh"),
            ("ext::sh -c x", "space"),
            ("ext::x", "not https or ssh"),
            ("/srv/repo.git", "local path"),
            ("../repo", "local path"),
            ("https://user:pw@github.com/a/b", "credentials"),
            ("ssh://-oProxyCommand=x@h/a", "user"),
            ("ssh://h/-x", "option-like"),
            ("https://-h/a", "host"),
            ("https://github.com/a/../b", "segment"),
            ("https://github.com/a?b", "segment"),
            ("https://github.com", "no path"),
            ("https://github.com:0/a", "port"),
            ("https://github.com:+22/a", "port"),
            ("https://github.com:022/a", "port"),
            ("ssh://[::1]/a", "IPv6"),
            ("h:1:a", "segment"),
        ] {
            let e = Remote::parse(text).unwrap_err();
            assert!(e.contains(why), "{text}: {e}");
        }
    }

    #[test]
    fn admission_is_exact_or_a_prefix_of_whole_segments() {
        let remote = |text| Remote::parse(text).unwrap();
        let exact = Admission::parse("https://github.com/timmydo/td").unwrap();
        assert!(exact.admits(&remote("https://github.com/timmydo/td.git")));
        assert!(exact.admits(&remote("https://GITHUB.com:443/timmydo/td/")));
        assert!(!exact.admits(&remote("git@github.com:timmydo/td")));
        assert!(!exact.admits(&remote("https://github.com/timmydo/td2")));
        let prefix = Admission::parse("github.com/timmydo").unwrap();
        assert!(prefix.admits(&remote("https://github.com/timmydo/td")));
        assert!(prefix.admits(&remote("git@github.com:timmydo/other.git")));
        assert!(!prefix.admits(&remote("https://github.com/timmydoX/td")));
        assert!(!prefix.admits(&remote("https://gitlab.com/timmydo/td")));
        let absolute = Admission::parse("ssh://git@github.com/timmydo/td").unwrap();
        assert!(!absolute.admits(&remote("git@github.com:timmydo/td")));
        let dotgit = Admission::parse("github.com/timmydo/td.git").unwrap();
        assert!(dotgit.admits(&remote("https://github.com/timmydo/td")));
        let host = Admission::parse("example.org").unwrap();
        assert!(host.admits(&remote("https://example.org/any/thing")));
        assert!(Admission::parse("github.com/../x").is_err());
        assert!(Admission::parse("http://github.com/a").is_err());
        // Two remotes never share a store.
        let a = remote("https://github.com/timmydo/td").store_name();
        let b = remote("git@github.com:timmydo/td").store_name();
        assert!(a.starts_with("github.com-timmydo-td-") && a != b, "{a} {b}");
    }

    #[test]
    fn branch_names_are_gits_and_never_options() {
        for good in ["main", "agent", "feature/x-1", "v1.2", "a@b"] {
            assert_eq!(branch_name(good), Ok(good));
        }
        for bad in [
            "", "-x", "+x", ":x", "/x", "x/", "x.", "x.lock", "a/.b", "a..b", "a@{1}", "a//b",
            "a b", "a~1", "a^", "a:b", "a?", "a*", "a[", "a\\b", "@", "a\u{7f}", "HEAD",
        ] {
            assert!(branch_name(bad).is_err(), "{bad:?}");
        }
    }

    /// Plain git over `dir`, the test's own, for the upstream fixture.
    fn upstream(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A local remote: file transport, admitted only in tests.
    fn local(dir: &Path) -> Remote {
        Remote {
            local: Some(dir.to_path_buf()),
            ..Remote::parse("https://local/up").unwrap()
        }
    }

    /// Whether a `git` is on PATH: the in-sandbox gate's toolchain has
    /// none, and the host preflight, which has one, runs these.
    pub(crate) fn have_git() -> bool {
        let found = Command::new("git").arg("--version").output().is_ok();
        if !found {
            eprintln!("skipped: no git on PATH; the host preflight runs this test");
        }
        found
    }

    #[test]
    fn the_store_fetches_in_the_fixed_shape_and_reads_a_base() {
        if !have_git() {
            return;
        }
        let scratch = Scratch::new("git-store");
        let root = scratch.state().root().to_path_buf();
        let up = root.join("up");
        std::fs::create_dir_all(&up).unwrap();
        upstream(&up, &["init", "--quiet"]);
        std::fs::write(up.join("AGENTS.md"), "be careful\n").unwrap();
        std::fs::create_dir_all(up.join(".td-agent")).unwrap();
        std::fs::write(up.join(".td-agent/rules"), "allow x\n").unwrap();
        std::os::unix::fs::symlink("AGENTS.md", up.join("link")).unwrap();
        upstream(&up, &["add", "."]);
        upstream(&up, &["commit", "--quiet", "-m", "one"]);
        // The human's global configuration: an identity and a helper are
        // copied, a hooks path, a redirect and an alias are not.
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".gitconfig"),
            "[user]\n\tname = Human\n\temail = h@example.org\n[credential]\n\thelper = store\n\
             [credential \"https://example.org\"]\n\thelper = cache\n\
             [core]\n\thooksPath = /evil\n[url \"https://evil.example/\"]\n\tinsteadOf = /\n\
             [alias]\n\tfetch = !touch /tmp/pwned\n",
        )
        .unwrap();
        let mut env = kept_env();
        env.retain(|(k, _)| k != "HOME");
        env.push(("HOME".into(), home.clone().into()));
        env.push((
            "GIT_CONFIG_PARAMETERS".into(),
            "'core.hooksPath'='/evil'".into(),
        ));
        let mut worker = Worker::new(&root.join("worker"), &env).unwrap();
        let copied = std::fs::read_to_string(root.join("worker/gitconfig")).unwrap();
        assert!(
            copied.contains("name = Human") && copied.contains("helper = store"),
            "{copied}"
        );
        assert!(copied.contains("helper = cache"), "{copied}");
        assert!(
            !copied.contains("evil") && !copied.contains("alias"),
            "{copied}"
        );
        assert!(worker.env.iter().all(|(k, _)| k != "GIT_CONFIG_PARAMETERS"));
        let remote = local(&up);
        let stores = root.join("store");
        let store = worker.store(&stores, &remote).unwrap();
        // Without the test's file transport, a local remote is refused.
        let refused = worker.fetch(&store, &remote).unwrap_err();
        assert!(refused.contains("not allowed"), "{refused}");
        worker.file = true;
        // The store's own hooks never run: a fetch updates references.
        let hook = store.join("hooks/reference-transaction");
        let ran = root.join("hook-ran");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", ran.display())).unwrap();
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        worker.fetch(&store, &remote).unwrap();
        assert!(!ran.exists(), "a store hook ran");
        let config = std::fs::read_to_string(store.join("config")).unwrap();
        assert!(
            !config.contains("[remote"),
            "no remote is configured: {config}"
        );
        let id = worker.resolve(&store, "main").unwrap();
        let (name, text) = worker.instructions(&store, &id).unwrap().unwrap();
        assert_eq!(
            (name, text.as_slice()),
            ("AGENTS.md", b"be careful\n".as_slice())
        );
        assert_eq!(
            worker
                .read(&store, &id, ".td-agent/rules")
                .unwrap()
                .unwrap(),
            b"allow x\n"
        );
        assert_eq!(worker.read(&store, &id, "nothing").unwrap(), None);
        assert_eq!(
            worker.read(&store, &id, ".td-agent").unwrap(),
            None,
            "a tree"
        );
        assert_eq!(worker.read(&store, &id, "link").unwrap(), None, "a link");
        assert!(worker.read(&store, "main", "AGENTS.md").is_err());
        assert!(worker
            .resolve(&store, "nope")
            .unwrap_err()
            .contains("no branch"));
        assert!(worker.resolve(&store, "-x").is_err());
        // A later fetch brings a new commit and prunes a deleted branch;
        // a file past the bound is refused.
        upstream(&up, &["branch", "gone"]);
        worker.fetch(&store, &remote).unwrap();
        assert!(worker.resolve(&store, "gone").is_ok());
        upstream(&up, &["branch", "-D", "gone"]);
        std::fs::write(up.join("big"), vec![b'x'; MAX_FILE as usize + 1]).unwrap();
        upstream(&up, &["add", "big"]);
        upstream(&up, &["commit", "--quiet", "-m", "two"]);
        worker.fetch(&store, &remote).unwrap();
        assert!(worker.resolve(&store, "gone").is_err(), "pruned");
        let two = worker.resolve(&store, "main").unwrap();
        assert_ne!(two, id);
        assert!(worker
            .read(&store, &two, "big")
            .unwrap_err()
            .contains("more than"));
        // The store is made once; a second call finds it, and refuses it
        // when it records another remote, as a collided name would.
        assert_eq!(worker.store(&stores, &remote).unwrap(), store);
        // A store whose `init` never ran, as after a crash, is finished.
        let other = Remote {
            local: Some(up.clone()),
            ..Remote::parse("https://local/up.git").unwrap()
        };
        let other_store = worker.store(&stores, &other).unwrap();
        assert_ne!(other_store, store, "`.git` names another repository");
        std::fs::remove_file(other_store.join("HEAD")).unwrap();
        assert_eq!(worker.store(&stores, &other).unwrap(), other_store);
        assert!(other_store.join("HEAD").is_file());
        std::fs::write(store.join("td-agent-remote"), "another").unwrap();
        assert!(worker
            .store(&stores, &remote)
            .unwrap_err()
            .contains("another remote"));
    }

    #[test]
    fn a_run_drains_a_long_error_and_ends_at_its_time() {
        if Command::new("sh").arg("-c").arg("true").status().is_err() {
            eprintln!("skipped: no sh on PATH");
            return;
        }
        let mut chatty = Command::new("sh");
        chatty.args(["-c", "head -c 200000 /dev/zero >&2 && echo ok"]);
        assert_eq!(run(&mut chatty, 64, LOCAL_TIME).unwrap(), b"ok\n");
        let mut slow = Command::new("sh");
        slow.args(["-c", "sleep 30"]);
        let started = Instant::now();
        let e = run(&mut slow, 64, Duration::from_millis(200)).unwrap_err();
        assert!(matches!(e, Failure::TimedOut(_)), "{e}");
        assert!(started.elapsed() < Duration::from_secs(10));
        // A process left holding the pipes does not hold the answer: the
        // shell's exit decides, after the grace.
        let mut left = Command::new("sh");
        left.args(["-c", "echo ok; sleep 30 & exit 0"]);
        let started = Instant::now();
        assert_eq!(
            run(&mut left, 64, Duration::from_secs(20)).unwrap(),
            b"ok\n"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(Worker::new(Path::new("relative"), &[]).is_err());
    }
}
