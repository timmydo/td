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
        self.branch_tip(store, base)?
            .ok_or_else(|| format!("the remote has no branch {base:?}"))
    }

    /// The commit the store's branch `branch` names, none when the
    /// remote has no such branch.
    fn branch_tip(&self, store: &Path, branch: &str) -> Result<Option<String>, String> {
        let branch = branch_name(branch)?;
        let mut parse = self.command(store);
        parse
            .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
            .arg(format!("refs/heads/{branch}^{{commit}}"));
        let out = match run(&mut parse, 256, LOCAL_TIME) {
            Ok(out) => out,
            Err(Failure::Exit(1, _)) => return Ok(None),
            Err(e) => return Err(format!("resolving {branch:?}: {e}")),
        };
        let id = String::from_utf8_lossy(&out).trim().to_string();
        object_id(&id)
            .then_some(Some(id))
            .ok_or_else(|| format!("git named {branch:?} oddly"))
    }

    /// The file at `path` in commit `id` of the store, none when the
    /// commit has no such regular file (a tree, a link and a submodule are
    /// none); refused past `MAX_FILE` bytes. Its entry is looked up once,
    /// the path literal, and the blob read by its id.
    pub fn read(&self, store: &Path, id: &str, path: &str) -> Result<Option<Vec<u8>>, String> {
        self.read_as(store, id, path, false)
    }

    /// As `read`, but a path there that is not a regular file is an
    /// error, not absent: a link or a tree where rules are looked for
    /// is a reason they were not read.
    pub fn read_regular(
        &self,
        store: &Path,
        id: &str,
        path: &str,
    ) -> Result<Option<Vec<u8>>, String> {
        self.read_as(store, id, path, true)
    }

    fn read_as(
        &self,
        store: &Path,
        id: &str,
        path: &str,
        regular: bool,
    ) -> Result<Option<Vec<u8>>, String> {
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
                if regular {
                    return Err(format!("{path} is not a regular file"));
                }
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

    /// The human's identity as the global configuration copied it, for
    /// a workspace repository's own configuration (DESIGN.md §8).
    pub fn identity(&self) -> Result<crate::repo::Identity, String> {
        let get = |key: &str| {
            let mut read = self.bare();
            read.env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["config", "--file"])
                .arg(&self.config)
                .args(["--get", key]);
            match run(&mut read, 4096, LOCAL_TIME) {
                Ok(out) => Ok(Some(
                    String::from_utf8_lossy(&out)
                        .trim_end_matches('\n')
                        .to_string(),
                )),
                // Exit 1 is git's "not set".
                Err(Failure::Exit(1, _)) => Ok(None),
                Err(e) => Err(format!("reading {key}: {e}")),
            }
        };
        Ok(crate::repo::Identity {
            name: get("user.name")?,
            email: get("user.email")?,
        })
    }

    /// The project instructions at the top of commit `id` (DESIGN.md
    /// §13): `AGENTS.md`, else `CLAUDE.md`, with the name it came from.
    pub fn instructions(
        &self,
        store: &Path,
        id: &str,
    ) -> Result<Option<(&'static str, Vec<u8>)>, String> {
        for name in crate::repo::INSTRUCTION_FILES.iter().copied() {
            if let Some(text) = self.read(store, id, name)? {
                return Ok(Some((name, text)));
            }
        }
        Ok(None)
    }
}

/// Why a git run failed.
#[derive(Debug)]
pub(crate) enum Failure {
    Start(std::io::Error),
    /// Its exit code, -1 when a signal ended it, and the start of what
    /// it said.
    Exit(i32, String),
    TooLong,
    TimedOut(Duration),
    /// What its output was handed to refused it.
    Sink(String),
}

/// The most bytes of pack a push's export carries (DESIGN.md §9): the
/// frames read from the maintenance instance, the file they are written
/// to, and the import's `--max-input-size`.
pub const MAX_PACK: u64 = 128 << 20;
/// How long an import may take.
const IMPORT_TIME: Duration = Duration::from_secs(600);

/// The most bytes of project instructions one store's answer carries, in
/// all: escaped as JSON escapes the worst text, within a frame.
pub const MAX_INSTRUCTIONS: usize = 128 * 1024;

/// The project instructions at each of `ids`, as `read` finds them there:
/// each commit's read once, within one bound for the answer, so it fits
/// its frame however JSON escapes it.
fn instructions_at(
    ids: &[String],
    mut read: impl FnMut(&str) -> Result<Option<(&'static str, Vec<u8>)>, String>,
) -> Vec<crate::repo::Instructions> {
    let mut done: Vec<(&String, crate::repo::Instructions)> = Vec::new();
    let mut carried = 0usize;
    for id in ids {
        if done.iter().any(|(at, _)| *at == id) {
            continue;
        }
        let found = match read(id) {
            Ok(None) => crate::repo::Instructions::Absent,
            Ok(Some((name, bytes))) => match String::from_utf8(bytes) {
                Ok(text) if carried.saturating_add(text.len()) > MAX_INSTRUCTIONS => {
                    crate::repo::Instructions::Unread {
                        why: format!(
                            "{name} is past {MAX_INSTRUCTIONS} bytes with the other bases' instructions"
                        ),
                    }
                }
                Ok(text) => {
                    carried = carried.saturating_add(text.len());
                    crate::repo::Instructions::Found {
                        name: name.to_string(),
                        text,
                    }
                }
                Err(_) => crate::repo::Instructions::Unread {
                    why: format!("{name} is not UTF-8"),
                },
            },
            Err(why) => crate::repo::Instructions::Unread { why },
        };
        done.push((id, found));
    }
    ids.iter()
        .map(|id| {
            done.iter()
                .find(|(at, _)| *at == id)
                .map_or(crate::repo::Instructions::Absent, |(_, found)| {
                    found.clone()
                })
        })
        .collect()
}

/// The repository rules at each of `ids` (DESIGN.md §11), as `read`
/// finds `.td-agent/rules` there: each commit's read and counted once
/// against one bound for the answer, as it crosses once, past which a
/// commit's are not read, so the human is asked.
fn rules_at(
    ids: &[String],
    mut read: impl FnMut(&str) -> Result<Option<Vec<u8>>, String>,
) -> Vec<crate::rules::Read> {
    let mut done: Vec<(&String, crate::rules::Read)> = Vec::new();
    let mut carried = 0usize;
    let mut out = Vec::new();
    for id in ids {
        if let Some((_, found)) = done.iter().find(|(at, _)| *at == id) {
            out.push(found.clone());
            continue;
        }
        let mut found = match read(id) {
            Ok(bytes) => crate::rules::Read::of(bytes),
            Err(why) => crate::rules::Read::unread(&why),
        };
        if carried.saturating_add(found.carried()) > crate::rules::MAX_CARRIED {
            found = crate::rules::Read::unread(&format!(
                "{} is past {} bytes with the other bases' rules",
                crate::rules::FILE,
                crate::rules::MAX_CARRIED
            ));
        }
        carried = carried.saturating_add(found.carried());
        done.push((id, found.clone()));
        out.push(found);
    }
    out
}

/// The window's store fetches (DESIGN.md §7, §9): one thread, which runs
/// the git worker outside any jail on the stores alone, one fetch at a
/// time, so two workspaces never fetch one store at once.
pub struct Service {
    jobs: mpsc::Sender<Job>,
    done: mpsc::Receiver<Done>,
}

/// What the store thread is asked.
enum Job {
    /// A conversation's: fetch `remote`'s store and resolve `bases`, with
    /// what preparing its workspace needs.
    Prepare {
        conversation: crate::store::Id,
        remote: Remote,
        bases: Vec<String>,
    },
    /// The window's own, every `fetch_interval`, or a conversation's
    /// `git_fetch`, `asker` naming it and its call: fetch `remote`'s store
    /// and resolve `bases`, the ones workspaces name.
    Refresh {
        remote: Remote,
        bases: Vec<String>,
        asker: Option<(crate::store::Id, u64)>,
    },
}

/// An answer, for the conversation that asked or for the window, with
/// the remote and the bases asked about.
pub enum Done {
    Prepared {
        conversation: crate::store::Id,
        remote: String,
        bases: Vec<String>,
        result: Result<crate::protocol::Fetched, String>,
    },
    /// Each base's commit, or why there is none, when the fetch was made;
    /// for the conversation and call that asked, if one did.
    Refreshed {
        remote: String,
        bases: Vec<String>,
        result: Result<Vec<Result<String, String>>, String>,
        asker: Option<(crate::store::Id, u64)>,
    },
}

impl Service {
    /// Starts the thread, whose worker keeps its files in `worker` and
    /// its stores in `stores`; a worker that cannot be made answers every
    /// ask with why.
    pub fn start(worker: PathBuf, stores: PathBuf, env: Vec<(String, OsString)>) -> Self {
        let (jobs, asked) = mpsc::channel::<Job>();
        let (tell, done) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("td-agent-stores".into())
            .spawn(move || {
                let made = Worker::new(&worker, &env);
                let unmade = |why: &String| format!("the git worker: {why}");
                let mut queue = std::collections::VecDeque::new();
                loop {
                    queue.extend(asked.try_iter());
                    if queue.is_empty() {
                        match asked.recv() {
                            Ok(job) => queue.push_back(job),
                            Err(_) => break,
                        }
                    }
                    let Some(job) = next(&mut queue) else {
                        continue;
                    };
                    let answer = match job {
                        Job::Prepare {
                            conversation,
                            remote,
                            bases,
                        } => Done::Prepared {
                            result: match &made {
                                Ok(made) => made.prepare(&stores, &remote, &bases),
                                Err(why) => Err(unmade(why)),
                            },
                            conversation,
                            remote: remote.url(),
                            bases,
                        },
                        Job::Refresh {
                            remote,
                            bases,
                            asker,
                        } => Done::Refreshed {
                            result: match &made {
                                Ok(made) => made.refresh(&stores, &remote, &bases),
                                Err(why) => Err(unmade(why)),
                            },
                            remote: remote.url(),
                            bases,
                            asker,
                        },
                    };
                    if tell.send(answer).is_err() {
                        break;
                    }
                }
            });
        if let Err(e) = spawned {
            eprintln!("td-agent: the store thread did not start: {e}");
        }
        Self { jobs, done }
    }

    /// Asks for `remote`'s store fetched and `bases` resolved there, for
    /// `conversation`; the answer comes from `answers`.
    pub fn ask(
        &self,
        conversation: crate::store::Id,
        remote: Remote,
        bases: Vec<String>,
    ) -> Result<(), String> {
        self.jobs
            .send(Job::Prepare {
                conversation,
                remote,
                bases,
            })
            .map_err(|_| "the store thread has ended".to_string())
    }

    /// Asks for `remote`'s store fetched in the background and `bases`
    /// resolved there, for the window; the answer comes from `answers`.
    pub fn refresh(&self, remote: Remote, bases: Vec<String>) -> Result<(), String> {
        self.jobs
            .send(Job::Refresh {
                remote,
                bases,
                asker: None,
            })
            .map_err(|_| "the store thread has ended".to_string())
    }

    /// `refresh`, for `conversation`'s `git_fetch`, call `call`, which
    /// waits on it, so it waits behind no queued background fetch.
    pub fn fetch_now(
        &self,
        conversation: crate::store::Id,
        call: u64,
        remote: Remote,
        bases: Vec<String>,
    ) -> Result<(), String> {
        self.jobs
            .send(Job::Refresh {
                remote,
                bases,
                asker: Some((conversation, call)),
            })
            .map_err(|_| "the store thread has ended".to_string())
    }

    /// The answers that have come.
    pub fn answers(&self) -> Vec<Done> {
        self.done.try_iter().collect()
    }
}

/// The job the store thread runs next: the first conversation's ask
/// waiting, a preparation or a `git_fetch`, so neither waits behind a
/// queued background fetch, only one already running; else the oldest
/// refresh.
fn next(queue: &mut std::collections::VecDeque<Job>) -> Option<Job> {
    let at = queue
        .iter()
        .position(|job| {
            matches!(
                job,
                Job::Prepare { .. } | Job::Refresh { asker: Some(_), .. }
            )
        })
        .unwrap_or(0);
    queue.remove(at)
}

impl Worker {
    /// The publish repository at `publish` (DESIGN.md §7, §9), made if
    /// need be: bare, its objects borrowing `store`'s, never mounted into
    /// a jail, and holding only what was imported for a push.
    pub fn publish(&self, publish: &Path, store: &Path) -> Result<(), String> {
        if !publish.is_absolute() {
            return Err(format!("{} is not absolute", publish.display()));
        }
        if !publish.join("HEAD").is_file() {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(publish)
                .map_err(|e| format!("{}: {e}", publish.display()))?;
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
                .arg(publish);
            // A second stage's `init` may hold git's lock; its
            // repository is ours.
            if let Err(e) = run(&mut init, MAX_TEXT as u64, LOCAL_TIME) {
                if !publish.join("HEAD").is_file() {
                    return Err(format!("making {}: {e}", publish.display()));
                }
            }
        }
        let objects = std::fs::canonicalize(store.join("objects"))
            .map_err(|e| format!("{}: {e}", store.display()))?;
        let alternates = publish.join("objects/info/alternates");
        // Written whole under a name of this call's own, so a second
        // stage's write never mixes with it.
        static CALLS: AtomicU64 = AtomicU64::new(0);
        let new = publish.join(format!(
            "objects/info/alternates.{}.{}",
            std::process::id(),
            CALLS.fetch_add(1, Ordering::Relaxed)
        ));
        let written = std::fs::write(&new, format!("{}\n", objects.display()))
            .and_then(|()| std::fs::rename(&new, &alternates));
        if written.is_err() {
            let _ = std::fs::remove_file(&new);
        }
        written.map_err(|e| format!("{}: {e}", alternates.display()))
    }

    /// The pack in file `pack`, a push's export, imported into publish
    /// repository `publish` (DESIGN.md §9): checked by `index-pack
    /// --strict`, which refuses a broken object and a link to one missing,
    /// so only objects cross and every one is whole; then `id` must be a
    /// commit there.
    pub fn import(&self, publish: &Path, pack: &Path, id: &str) -> Result<(), String> {
        if !object_id(id) {
            return Err(format!("{id:?} is not a full commit id"));
        }
        let file = std::fs::File::open(pack).map_err(|e| format!("{}: {e}", pack.display()))?;
        let size = file
            .metadata()
            .map_err(|e| format!("{}: {e}", pack.display()))?
            .len();
        if size > MAX_PACK {
            return Err(format!(
                "the export is {size} bytes, past the {MAX_PACK}-byte bound"
            ));
        }
        run(
            self.command(publish)
                .args([
                    "index-pack",
                    "--strict",
                    &format!("--max-input-size={MAX_PACK}"),
                    "--stdin",
                ])
                .stdin(file),
            MAX_TEXT as u64,
            IMPORT_TIME,
        )
        .map_err(|e| format!("importing the export: {e}"))?;
        let found = run(
            self.command(publish).args([
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &format!("{id}^{{commit}}"),
            ]),
            MAX_TEXT as u64,
            LOCAL_TIME,
        )
        .map_err(|e| match e {
            // `--quiet`: it is not there, or no commit.
            Failure::Exit(1, _) => format!("the import holds no commit {id}"),
            e => format!("finding {id} in the import: {e}"),
        })?;
        if String::from_utf8_lossy(&found).trim() != id {
            return Err(format!("the import holds no commit {id}"));
        }
        Ok(())
    }

    /// The store for `remote` under `stores`, made if need be and
    /// fetched, with `bases` resolved there and the human's identity.
    fn prepare(
        &self,
        stores: &Path,
        remote: &Remote,
        bases: &[String],
    ) -> Result<crate::protocol::Fetched, String> {
        let store = self.store(stores, remote)?;
        self.fetch(&store, remote)?;
        let ids = bases
            .iter()
            .map(|base| self.resolve(&store, base))
            .collect::<Result<Vec<_>, _>>()?;
        let instructions = instructions_at(&ids, |id| self.instructions(&store, id));
        let rules = rules_at(&ids, |id| self.read_regular(&store, id, crate::rules::FILE));
        Ok(crate::protocol::Fetched {
            identity: self.identity()?,
            ids,
            instructions,
            rules,
        })
    }

    /// The store for `remote` under `stores`, made if need be and
    /// fetched, then each of `bases` resolved there, or why not: a base
    /// upstream deleted fails alone.
    fn refresh(
        &self,
        stores: &Path,
        remote: &Remote,
        bases: &[String],
    ) -> Result<Vec<Result<String, String>>, String> {
        let store = self.store(stores, remote)?;
        self.fetch(&store, remote)?;
        Ok(bases
            .iter()
            .map(|base| self.resolve(&store, base))
            .collect())
    }
}

/// The most commits, paths, binary files and matches the evidence names;
/// the rest are counted.
const MAX_COMMITS: usize = 200;
const MAX_PATHS: usize = 500;
const MAX_FOUND: usize = 50;
/// The most bytes the scan reads in each of its passes; what is past it
/// is a match of its own, unscanned.
pub const MAX_SCANNED: u64 = 512 << 20;
/// The most bytes of the list of objects a push carries.
const MAX_OBJECTS: u64 = 64 << 20;
/// How long the evidence's git may take, each run.
const EVIDENCE_TIME: Duration = Duration::from_secs(600);
/// The longest line kept whole for the scan; a longer one is scanned in
/// pieces that overlap by `OVERLAP`, so no shape is cut in two.
const SCAN_LINE: usize = 64 * 1024;
pub(crate) const OVERLAP: usize = 512;
/// The longest subject named, in characters.
const SUBJECT: usize = 200;
/// How much of a commit object is kept for its subject.
const COMMIT_HEAD: usize = 64 * 1024;
/// How much of a blob git reads to call it binary: a NUL among them.
const SNIFF: usize = 8000;
/// The longest path record kept; a longer one's path is cut.
const RECORD: usize = 64 * 1024;
/// The longest header `cat-file` says before an object.
const OBJECT_HEADER: usize = 512;

/// What a push publishes, found in the publish repository outside any
/// jail (DESIGN.md §9, Pushing, step 3).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Evidence {
    /// Where the commit and the remote branch's tip meet, when the
    /// branch is there and they do.
    pub merge_base: Option<String>,
    /// The commits the push adds, newest first, by id and subject.
    pub commits: Vec<(String, String)>,
    /// How many more commits it adds than are named.
    pub more_commits: u64,
    /// Each path changed from the merge-base, else the base, with its
    /// lines added and removed, none for a binary file.
    pub paths: Vec<(String, Option<(u64, u64)>)>,
    pub more_paths: u64,
    /// Each binary file the push carries, by its path: every blob it
    /// adds that git would call binary, in whichever commit, though a
    /// later one removes it.
    pub binaries: Vec<String>,
    pub more_binaries: u64,
    /// Each credential shape found: its kind, the commit, and the path.
    pub found: Vec<Found>,
    pub more_found: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Found {
    pub kind: &'static str,
    /// The commit whose object or own diff holds it; none for a file's
    /// whole text or a directory's names, which commits share.
    pub commit: Option<String>,
    /// The file or directory, none for a commit's own object or the top
    /// directory.
    pub path: Option<String>,
}

impl Evidence {
    /// Whether the scan matched nothing: no credential shape, no binary
    /// file, nothing left unscanned.
    pub fn clean(&self) -> bool {
        self.found.is_empty()
            && self.more_found == 0
            && self.binaries.is_empty()
            && self.more_binaries == 0
    }
}

/// How the scan reads a text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    /// A diff: a hunk's header says how many of the lines after it are
    /// the file's, so those are told from headers by counting, never by
    /// how they look. Of the file's lines only the added ones are
    /// scanned, read without NULs too, as text in UTF-16 is; every
    /// header line is scanned. A commit starts at a line `\0commit <id>`,
    /// which no line of a file's can be, each carrying its `+`, `-` or
    /// space.
    Diff,
    /// An object as it is stored, every line scanned.
    Raw,
    /// A file's whole text, for private-key armour alone.
    Armour,
}

/// The scan of a text, a line at a time.
struct Scanner {
    mode: Mode,
    line: Vec<u8>,
    /// Whether the line kept is a long line's later piece: its first
    /// byte, which says what it is, then one byte of context, then what
    /// is left to scan.
    cut: bool,
    commit: Option<String>,
    path: Option<String>,
    /// The file's lines left in the hunk: old, then new.
    left: (u64, u64),
    found: Vec<Found>,
    more: u64,
}

impl Scanner {
    fn new(mode: Mode, found: Vec<Found>, more: u64) -> Self {
        Self {
            mode,
            line: Vec::new(),
            cut: false,
            commit: None,
            path: None,
            left: (0, 0),
            found,
            more,
        }
    }

    /// Ends the text read so far and starts another, read as `mode`.
    fn start(&mut self, mode: Mode, commit: Option<String>, path: Option<String>) {
        self.finish();
        self.mode = mode;
        self.commit = commit;
        self.path = path;
        self.left = (0, 0);
    }

    /// Takes the next of the text.
    fn take(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if b == b'\n' {
                self.end_line();
                continue;
            }
            self.line.push(b);
            // A line too long to keep is scanned so far, and its first
            // byte kept with its end.
            if self.line.len() >= SCAN_LINE {
                if self.scanned() {
                    self.scan(false);
                }
                let end = self
                    .line
                    .split_off(self.line.len().saturating_sub(OVERLAP + 1));
                self.line.truncate(1);
                self.line.extend_from_slice(&end);
                self.cut = true;
            }
        }
    }

    /// Ends the text: a last line without its newline.
    fn finish(&mut self) {
        if !self.line.is_empty() {
            self.end_line();
        }
    }

    fn in_hunk(&self) -> bool {
        self.mode == Mode::Diff && (self.left.0 > 0 || self.left.1 > 0)
    }

    /// Whether the line kept is one the scan reads.
    fn scanned(&self) -> bool {
        !self.in_hunk() || self.line.first() == Some(&b'+')
    }

    fn end_line(&mut self) {
        let header = self.mode == Mode::Diff && !self.in_hunk();
        // A file's path is known at its `+++` line: from its `diff --git`
        // line on, or a header too long to read, none is named.
        if header && (self.cut || self.line.starts_with(b"diff --git ")) {
            self.path = None;
        }
        if self.scanned() {
            self.scan(true);
        }
        if self.mode == Mode::Diff {
            self.follow();
        }
        self.line.clear();
        self.cut = false;
    }

    /// Follows a diff's state past the line kept.
    fn follow(&mut self) {
        if let Some(rest) = self.line.strip_prefix(b"\0commit ") {
            self.commit = Some(String::from_utf8_lossy(rest).trim().to_string());
            self.path = None;
            self.left = (0, 0);
        } else if self.in_hunk() {
            let (old, new) = &mut self.left;
            match self.line.first() {
                Some(b'+') => *new = new.saturating_sub(1),
                Some(b'-') => *old = old.saturating_sub(1),
                Some(b' ') => {
                    *old = old.saturating_sub(1);
                    *new = new.saturating_sub(1);
                }
                // `\ No newline at end of file` counts as neither.
                Some(b'\\') => {}
                // Not a file's line: the hunk was not as said.
                _ => self.left = (0, 0),
            }
        } else if let Some(path) = self.line.strip_prefix(b"+++ ").filter(|_| !self.cut) {
            let path = String::from_utf8_lossy(path);
            let path = path
                .strip_prefix('"')
                .and_then(|path| path.strip_suffix('"'))
                .unwrap_or(&path);
            let path = path.strip_prefix("b/").unwrap_or(path);
            self.path = Some(crate::tools::visible(path));
        } else if let Some(left) = hunk(&self.line) {
            self.left = left;
        }
    }

    /// Scans the line kept, `whole` when it has ended.
    fn scan(&mut self, whole: bool) {
        let start = if self.cut { 2 } else { 0 };
        let line = &self.line;
        let kind = match self.mode {
            Mode::Armour => crate::scan::armour(line, start),
            Mode::Raw => crate::scan::within(line, start, whole),
            Mode::Diff => crate::scan::within(line, start, whole).or_else(|| {
                if !line.contains(&0) {
                    return None;
                }
                let skipped = line.iter().take(start).filter(|b| **b == 0).count();
                let bare: Vec<u8> = line.iter().copied().filter(|b| *b != 0).collect();
                crate::scan::within(&bare, start.saturating_sub(skipped), whole)
            }),
        };
        if let Some(kind) = kind {
            self.note(kind);
        }
    }

    /// Notes a match of `kind` where the scan is, once.
    fn note(&mut self, kind: &'static str) {
        let found = Found {
            kind,
            commit: self.commit.clone(),
            path: self.path.clone(),
        };
        if self.found.contains(&found) {
            return;
        }
        if self.found.len() < MAX_FOUND {
            self.found.push(found);
        } else {
            self.more = self.more.saturating_add(1);
        }
    }
}

/// A hunk header's counts, `@@ -start[,old] +start[,new] @@`: how many of
/// the file's old and new lines follow it.
fn hunk(line: &[u8]) -> Option<(u64, u64)> {
    let text = std::str::from_utf8(line).ok()?;
    let rest = text.strip_prefix("@@ -")?;
    let (ranges, _) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let count = |range: &str| {
        let (start, count) = range.split_once(',').unwrap_or((range, "1"));
        start.parse::<u64>().ok()?;
        count.parse::<u64>().ok()
    };
    Some((count(old)?, count(new)?))
}

/// Where `cat-file --batch`'s answer is.
enum At {
    /// In an object's header line.
    Header,
    /// In its contents, this many bytes left.
    Body(u64),
    /// At the newline after them.
    Newline,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Commit,
    Tree,
    Blob,
    Other,
}

/// The objects a push carries, read as `cat-file --batch` says them, in
/// the order `rev-list --objects` named them, commits first: each handed
/// to the scan as it comes, a commit or a tree as it is stored, so no
/// encoding, quoting or header git would show differently hides what
/// is published; a blob for private-key armour, and as binary when git
/// would call it so.
struct Objects<'a> {
    names: &'a std::collections::HashMap<String, String>,
    scanner: Scanner,
    header: Vec<u8>,
    at: At,
    kind: Kind,
    id: String,
    /// A commit's start, for its subject.
    head: Vec<u8>,
    sniffed: usize,
    binary: bool,
    commits: Vec<(String, String)>,
    more_commits: u64,
    binaries: Vec<String>,
    more_binaries: u64,
    named: std::collections::HashSet<String>,
}

impl<'a> Objects<'a> {
    fn new(names: &'a std::collections::HashMap<String, String>) -> Self {
        Self {
            names,
            scanner: Scanner::new(Mode::Raw, Vec::new(), 0),
            header: Vec::new(),
            at: At::Header,
            kind: Kind::Other,
            id: String::new(),
            head: Vec::new(),
            sniffed: 0,
            binary: false,
            commits: Vec::new(),
            more_commits: 0,
            binaries: Vec::new(),
            more_binaries: 0,
            named: std::collections::HashSet::new(),
        }
    }

    /// Takes the next of `cat-file`'s answer.
    fn take(&mut self, mut bytes: &[u8]) -> Result<(), String> {
        while !bytes.is_empty() {
            match self.at {
                At::Header => {
                    let end = bytes.iter().position(|b| *b == b'\n');
                    let (part, rest) = bytes
                        .split_at_checked(end.unwrap_or(bytes.len()))
                        .unwrap_or((bytes, &[]));
                    if self.header.len().saturating_add(part.len()) > OBJECT_HEADER {
                        return Err("cat-file said a header too long".into());
                    }
                    self.header.extend_from_slice(part);
                    bytes = rest;
                    if end.is_some() {
                        bytes = bytes.get(1..).unwrap_or_default();
                        self.open()?;
                    }
                }
                At::Body(left) => {
                    let n = usize::try_from(left).unwrap_or(usize::MAX).min(bytes.len());
                    let (part, rest) = bytes.split_at_checked(n).unwrap_or((bytes, &[]));
                    self.body(part);
                    bytes = rest;
                    let left = left.saturating_sub(n as u64);
                    self.at = if left == 0 {
                        self.close();
                        At::Newline
                    } else {
                        At::Body(left)
                    };
                }
                At::Newline => {
                    bytes = bytes.get(1..).unwrap_or_default();
                    self.at = At::Header;
                }
            }
        }
        Ok(())
    }

    /// Opens the object the header kept names.
    fn open(&mut self) -> Result<(), String> {
        let header = String::from_utf8_lossy(&std::mem::take(&mut self.header)).into_owned();
        let mut words = header.split(' ');
        let (id, kind, size) = (words.next(), words.next(), words.next());
        let size = size.and_then(|size| size.parse::<u64>().ok());
        let (id, kind, size) = match (id, kind, size) {
            (Some(id), Some(kind), Some(size)) if object_id(id) => (id, kind, size),
            // A line of the list that was a path's, not an object's.
            (Some(id), Some("missing"), None) if object_id(id) => return Ok(()),
            _ => {
                return Err(format!(
                    "cat-file said {:?}",
                    crate::tools::visible(&header)
                ))
            }
        };
        self.id = id.to_string();
        self.kind = match kind {
            "commit" => Kind::Commit,
            "tree" => Kind::Tree,
            "blob" => Kind::Blob,
            _ => Kind::Other,
        };
        let path = self
            .names
            .get(id)
            .filter(|name| !name.is_empty())
            .map(|name| crate::tools::visible(name));
        let (mode, commit, path) = match self.kind {
            Kind::Commit => (Mode::Raw, Some(self.id.clone()), None),
            Kind::Blob => (Mode::Armour, None, path),
            Kind::Tree | Kind::Other => (Mode::Raw, None, path),
        };
        self.scanner.start(mode, commit, path);
        self.head.clear();
        self.sniffed = 0;
        self.binary = false;
        self.at = At::Body(size);
        Ok(())
    }

    fn body(&mut self, part: &[u8]) {
        if self.kind == Kind::Commit {
            let room = COMMIT_HEAD.saturating_sub(self.head.len()).min(part.len());
            self.head
                .extend_from_slice(part.get(..room).unwrap_or_default());
        }
        if self.kind == Kind::Blob && self.sniffed < SNIFF {
            let look = SNIFF.saturating_sub(self.sniffed).min(part.len());
            self.binary |= part.get(..look).unwrap_or_default().contains(&0);
            self.sniffed = self.sniffed.saturating_add(look);
        }
        self.scanner.take(part);
    }

    fn close(&mut self) {
        self.scanner.finish();
        match self.kind {
            Kind::Commit if self.commits.len() < MAX_COMMITS => {
                self.commits.push((self.id.clone(), subject(&self.head)));
            }
            Kind::Commit => self.more_commits = self.more_commits.saturating_add(1),
            Kind::Blob if self.binary => {
                let path = self
                    .names
                    .get(&self.id)
                    .map_or_else(|| self.id.clone(), |name| crate::tools::visible(name));
                if self.named.insert(path.clone()) {
                    if self.binaries.len() < MAX_PATHS {
                        self.binaries.push(path);
                    } else {
                        self.more_binaries = self.more_binaries.saturating_add(1);
                    }
                }
            }
            _ => {}
        }
    }
}

/// A commit object's subject: the first line of its message, as stored.
fn subject(head: &[u8]) -> String {
    let text = String::from_utf8_lossy(head);
    let message = text.split_once("\n\n").map_or("", |(_, message)| message);
    let line = message.lines().next().unwrap_or_default();
    crate::tools::visible(line.trim())
        .chars()
        .take(SUBJECT)
        .collect()
}

impl Worker {
    /// The evidence of pushing commit `id` from publish repository
    /// `publish`, its export imported, onto a branch whose remote tip is
    /// `tip`, if it is there, from `base`, both commits of the store
    /// (DESIGN.md §9, Pushing, step 3). It reads the objects the push
    /// carries as they are stored, then every commit's own diff, text,
    /// without textconv or external diff, its attributes from an empty
    /// tree, so nothing a commit carries hides content from the scan.
    pub fn evidence(
        &self,
        publish: &Path,
        id: &str,
        base: &str,
        tip: Option<&str>,
    ) -> Result<Evidence, String> {
        for commit in [Some(id), Some(base), tip].into_iter().flatten() {
            if !object_id(commit) {
                return Err(format!("{commit:?} is not a full commit id"));
            }
        }
        let text = |command: &mut Command, limit: u64| {
            run(command, limit, EVIDENCE_TIME).map(|out| String::from_utf8_lossy(&out).into_owned())
        };
        let empty = text(
            self.command(publish)
                .args(["hash-object", "-t", "tree", "--stdin"])
                .stdin(Stdio::null()),
            MAX_TEXT as u64,
        )
        .map_err(|e| format!("the empty tree: {e}"))?
        .trim()
        .to_string();
        let attributes = format!("--attr-source={empty}");
        // What the push adds: neither the base's nor the remote's.
        let mut range = vec![id.to_string(), format!("^{base}")];
        range.extend(tip.map(|tip| format!("^{tip}")));
        let merge_base = match tip {
            None => None,
            Some(tip) => match run(
                self.command(publish)
                    .args(["merge-base", "--end-of-options", id, tip]),
                MAX_TEXT as u64,
                EVIDENCE_TIME,
            ) {
                Ok(out) => Some(String::from_utf8_lossy(&out).trim().to_string()),
                // Unrelated histories meet nowhere.
                Err(Failure::Exit(1, _)) => None,
                Err(e) => return Err(format!("the merge-base: {e}")),
            },
        };
        let from = merge_base.clone().unwrap_or_else(|| base.to_string());
        let mut evidence = Evidence {
            merge_base,
            ..Evidence::default()
        };
        let count = text(
            self.command(publish)
                .args(["rev-list", "--count", "--end-of-options"])
                .args(&range),
            MAX_TEXT as u64,
        )
        .map_err(|e| format!("counting the commits: {e}"))?;
        let total: u64 = count
            .trim()
            .parse()
            .map_err(|_| "counting the commits: no number")?;
        // The objects, as they are stored.
        let listed = run(
            self.command(publish)
                .args(["rev-list", "--objects", "--end-of-options"])
                .args(&range),
            MAX_OBJECTS,
            EVIDENCE_TIME,
        );
        let mut names = std::collections::HashMap::new();
        let mut wanted = Vec::new();
        let (found, more) = match listed {
            Ok(out) => {
                for line in out.split(|b| *b == b'\n') {
                    let (object, name) = match line.iter().position(|b| *b == b' ') {
                        Some(at) => (line.get(..at), line.get(at + 1..)),
                        None => (Some(line), None),
                    };
                    let object = String::from_utf8_lossy(object.unwrap_or_default()).into_owned();
                    if !object_id(&object) {
                        continue;
                    }
                    wanted.extend_from_slice(object.as_bytes());
                    wanted.push(b'\n');
                    if let Some(name) = name {
                        names
                            .entry(object)
                            .or_insert_with(|| String::from_utf8_lossy(name).into_owned());
                    }
                }
                let mut objects = Objects::new(&names);
                let mut cat = self.command(publish);
                cat.args(["cat-file", "--batch"]);
                let read = run_into(
                    &mut cat,
                    Some(wanted),
                    MAX_SCANNED,
                    EVIDENCE_TIME,
                    &mut |bytes| objects.take(bytes),
                );
                objects.scanner.finish();
                match read {
                    Ok(()) => {}
                    Err(Failure::TooLong) => objects.scanner.note(UNSCANNED),
                    Err(e) => return Err(format!("reading the objects: {e}")),
                }
                evidence.commits = objects.commits;
                evidence.binaries = objects.binaries;
                evidence.more_binaries = objects.more_binaries;
                (objects.scanner.found, objects.scanner.more)
            }
            Err(Failure::TooLong) => {
                let mut scanner = Scanner::new(Mode::Raw, Vec::new(), 0);
                scanner.note(UNSCANNED);
                (scanner.found, scanner.more)
            }
            Err(e) => return Err(format!("listing the objects: {e}")),
        };
        evidence.more_commits = total.saturating_sub(evidence.commits.len() as u64);
        // Every commit's own diff, a merge's against its first parent, so
        // what one commit adds and a later one removes is still found;
        // only what is added, so what was there already is not.
        let mut scanner = Scanner::new(Mode::Diff, found, more);
        let mut log = self.command(publish);
        log.arg(&attributes).args([
            "log",
            "--no-show-signature",
            "--no-color",
            "--format=%x00commit %H",
            "-p",
            "--diff-merges=first-parent",
            "--text",
            "--no-textconv",
            "--no-ext-diff",
            "--no-renames",
            "--end-of-options",
        ]);
        log.args(&range);
        let scanned = run_into(&mut log, None, MAX_SCANNED, EVIDENCE_TIME, &mut |bytes| {
            scanner.take(bytes);
            Ok(())
        });
        scanner.finish();
        match scanned {
            Ok(()) => {}
            Err(Failure::TooLong) => scanner.note(UNSCANNED),
            Err(e) => return Err(format!("scanning the commits: {e}")),
        }
        evidence.found = scanner.found;
        evidence.more_found = scanner.more;
        // The paths, from where the push and the remote meet.
        let mut diff = self.command(publish);
        diff.arg(&attributes).args([
            "diff",
            "--numstat",
            "-z",
            "--no-renames",
            "--no-textconv",
            "--no-ext-diff",
            "--end-of-options",
            &from,
            id,
        ]);
        let mut record = Vec::new();
        let (paths, more_paths) = (&mut evidence.paths, &mut evidence.more_paths);
        run_into(&mut diff, None, MAX_SCANNED, EVIDENCE_TIME, &mut |bytes| {
            for &b in bytes {
                if b != 0 {
                    if record.len() < RECORD {
                        record.push(b);
                    }
                    continue;
                }
                if let Some(entry) = numstat(&record) {
                    if paths.len() < MAX_PATHS {
                        paths.push(entry);
                    } else {
                        *more_paths = more_paths.saturating_add(1);
                    }
                }
                record.clear();
            }
            Ok(())
        })
        .map_err(|e| format!("the paths changed: {e}"))?;
        Ok(evidence)
    }
}

/// How long a push may take.
const PUSH_TIME: Duration = Duration::from_secs(600);

/// A push staged (DESIGN.md §9, Pushing, steps 2 and 3): the remote
/// branch's tip when it is there, as the store last fetched it, and the
/// evidence against it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Staged {
    pub tip: Option<String>,
    pub evidence: Evidence,
}

/// What a push sends (DESIGN.md §9, Pushing, step 5): one commit to one
/// branch, the remote's id expected there when forced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Push {
    pub id: String,
    pub branch: String,
    /// For a force push, the id the remote's branch must still name,
    /// carried by `--force-with-lease`.
    pub lease: Option<String>,
}

impl Push {
    /// The one refspec pushed, `<id>:refs/heads/<branch>`, checked: the
    /// id a full object id, the branch one `branch_name` admits, which
    /// starts with none of `+`, `:` and `-`, and the lease an id too.
    /// Neither is the null id, which git reads as no commit: pushed, a
    /// deletion; as a lease, a branch that must not be there.
    pub fn refspec(&self) -> Result<String, String> {
        let commit = |id: &str| object_id(id) && id.bytes().any(|b| b != b'0');
        if !commit(&self.id) {
            return Err(format!("{:?} is not a full commit id", self.id));
        }
        let branch = branch_name(&self.branch)?;
        // `refs/heads/refs/heads/x` is a branch git makes, but not the
        // one a reader of `refs/heads/x` would think.
        if branch.starts_with("refs/") {
            return Err(format!("{branch:?} names a ref, not a branch"));
        }
        if let Some(lease) = self.lease.as_deref().filter(|lease| !commit(lease)) {
            return Err(format!("{lease:?} is not a full commit id"));
        }
        Ok(format!("{}:refs/heads/{branch}", self.id))
    }
}

impl Worker {
    /// Stages pushing commit `id`, from `base`, to `remote`'s branch
    /// `branch`: fetches the store under `stores`, so the branch's tip is
    /// the remote's now; makes the publish repository `publish` over it
    /// if need be; imports `pack`, which must hold `id`; and computes the
    /// evidence against that tip.
    #[allow(clippy::too_many_arguments)]
    pub fn stage(
        &self,
        stores: &Path,
        publish: &Path,
        remote: &Remote,
        pack: &Path,
        id: &str,
        base: &str,
        branch: &str,
    ) -> Result<Staged, String> {
        let branch = branch_name(branch)?;
        let store = self.store(stores, remote)?;
        self.fetch(&store, remote)?;
        let tip = self.branch_tip(&store, branch)?;
        self.publish(publish, &store)?;
        self.import(publish, pack, id)?;
        let evidence = self.evidence(publish, id, base, tip.as_deref())?;
        Ok(Staged { tip, evidence })
    }

    /// Pushes `push` from the publish repository to `remote` with the
    /// human's credentials, its hooks off, and answers with what git and
    /// the remote said: the ref's status and the remote's message.
    pub fn push(&self, publish: &Path, remote: &Remote, push: &Push) -> Result<String, String> {
        let refspec = push.refspec()?;
        // Through the alternates every object of the store is there: the
        // id must be a commit's, not a tag's or a tree's.
        let mut peel = self.command(publish);
        peel.args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
            .arg(format!("{}^{{commit}}", push.id));
        let peeled = run(&mut peel, 256, LOCAL_TIME)
            .map(|out| String::from_utf8_lossy(&out).trim().to_string());
        if peeled.ok().as_deref() != Some(push.id.as_str()) {
            return Err(format!("{} is not a commit to push", push.id));
        }
        let mut command = self.command(publish);
        command.args([
            "push",
            "--porcelain",
            "--no-verify",
            "--no-signed",
            "--no-follow-tags",
            "--no-recurse-submodules",
            "--no-atomic",
        ]);
        if let Some(lease) = &push.lease {
            command.arg(format!(
                "--force-with-lease=refs/heads/{}:{lease}",
                push.branch
            ));
        }
        command
            .arg("--end-of-options")
            .arg(remote.url())
            .arg(&refspec);
        let mut out = Vec::new();
        let pushed = run_heard(&mut command, None, 64 * 1024, PUSH_TIME, &mut |bytes| {
            out.extend_from_slice(bytes);
            Ok(())
        });
        let out = crate::tools::visible(String::from_utf8_lossy(&out).trim());
        let both = |said: &str| match (out.is_empty(), said.is_empty()) {
            (_, true) => out.clone(),
            (true, false) => said.to_string(),
            (false, false) => format!("{out}\n{said}"),
        };
        match pushed {
            Ok(said) => Ok(both(&said)),
            // git says 1 when the remote refused a ref, more when it
            // could not push at all.
            Err(Failure::Exit(1, said)) => Err(format!("the push was refused: {}", both(&said))),
            Err(Failure::Exit(_, said)) => Err(format!(
                "pushing to {} failed: {}",
                remote.url(),
                both(&said)
            )),
            Err(e) => Err(format!("pushing to {}: {e}", remote.url())),
        }
    }
}

/// What a match past the scan's bound is called.
const UNSCANNED: &str = "more than the scan reads, unscanned";

/// One record of `diff --numstat -z`: a path with its lines added and
/// removed, none for a binary file.
fn numstat(record: &[u8]) -> Option<(String, Option<(u64, u64)>)> {
    let text = String::from_utf8_lossy(record);
    let mut fields = text.splitn(3, '\t');
    let added = fields.next()?;
    let removed = fields.next()?;
    let path = fields.next()?;
    let lines = match (added.parse::<u64>(), removed.parse::<u64>()) {
        (Ok(a), Ok(r)) => Some((a, r)),
        _ => None,
    };
    Some((crate::tools::visible(path), lines))
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start(e) => write!(f, "git did not start: {e}"),
            Self::Exit(code, said) if said.is_empty() => write!(f, "git exited {code}"),
            Self::Exit(code, said) => write!(f, "git exited {code}: {said}"),
            Self::TooLong => write!(f, "git said more than was asked for"),
            Self::TimedOut(time) => write!(f, "git took more than {} seconds", time.as_secs()),
            Self::Sink(why) => write!(f, "its output: {why}"),
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
fn reader(
    mut from: impl Read + Send + 'static,
    tell: mpsc::SyncSender<Piece>,
    out: bool,
    cap: u64,
) {
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

/// How long the pipes may stay quiet once git has exited: a helper it
/// left behind can hold them.
const GRACE: Duration = Duration::from_secs(1);
/// The most pieces of git's output read ahead of the sink.
const PIECES: usize = 16;

/// Runs `command` for at most `time`, returning at most `limit` bytes of
/// its standard output. Its standard error is read to its end, so a git
/// that says much never dies on a closed pipe, and its start kept for the
/// reason. git's own exit decides: once it has exited, what its pipes
/// gave within `GRACE` is its answer. Past `time`, or past `limit`, git
/// is killed and the readers left to end with whatever it started.
pub(crate) fn run(command: &mut Command, limit: u64, time: Duration) -> Result<Vec<u8>, Failure> {
    run_fed(command, None, limit, time)
}

/// `run`, with `input` on git's standard input, written on a thread so a
/// git that reads none cannot stall it.
pub(crate) fn run_fed(
    command: &mut Command,
    input: Option<Vec<u8>>,
    limit: u64,
    time: Duration,
) -> Result<Vec<u8>, Failure> {
    let mut out = Vec::new();
    run_into(command, input, limit, time, &mut |bytes| {
        out.extend_from_slice(bytes);
        Ok(())
    })?;
    Ok(out)
}

/// `run_fed`, its standard output handed to `sink` as it comes rather
/// than kept, so what it says may be larger than memory should hold; a
/// sink's refusal kills git.
pub(crate) fn run_into(
    command: &mut Command,
    input: Option<Vec<u8>>,
    limit: u64,
    time: Duration,
    sink: &mut dyn FnMut(&[u8]) -> Result<(), String>,
) -> Result<(), Failure> {
    run_heard(command, input, limit, time, sink)
        .map(drop)
        .map_err(|failure| match failure {
            Failure::Exit(code, said) => Failure::Exit(code, said.chars().take(300).collect()),
            other => other,
        })
}

/// `run_into`, answering with the start of what git said on standard
/// error when it succeeds too, and all of that start when it fails, not
/// cut as a reason: a remote's message, made visible.
fn run_heard(
    command: &mut Command,
    input: Option<Vec<u8>>,
    limit: u64,
    time: Duration,
    sink: &mut dyn FnMut(&[u8]) -> Result<(), String>,
) -> Result<String, Failure> {
    let now = Instant::now();
    let deadline = now.checked_add(time).unwrap_or(now);
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(Failure::Start)?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = std::io::Write::write_all(&mut stdin, &input);
        });
    }
    // Bounded, so a slow sink holds git back rather than its output
    // piling up here.
    let (tell, heard) = mpsc::sync_channel(PIECES);
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
    let (mut told, mut err) = (0u64, Vec::new());
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
        // Once git has exited, its pipes have `GRACE` from the last of
        // them heard: what it said is all taken, however slow the sink,
        // and only a pipe a helper holds quiet is left.
        let until = exited.map_or(deadline, |(_, grace)| grace.min(deadline));
        if exited.is_some() && now >= until {
            break;
        }
        let wait = until
            .saturating_duration_since(now)
            .min(Duration::from_millis(20));
        let piece = heard.recv_timeout(wait);
        // Standard output alone carries the answer: a helper's chatter
        // on standard error renews nothing.
        if let (Ok(Piece::Out(_)), Some((_, grace))) = (&piece, &mut exited) {
            let now = Instant::now();
            *grace = now.checked_add(GRACE).unwrap_or(now);
        }
        match piece {
            Ok(Piece::Out(bytes)) => {
                told = told.saturating_add(bytes.len() as u64);
                if told > limit {
                    return stop(&mut child, Failure::TooLong);
                }
                if let Err(why) = sink(&bytes) {
                    return stop(&mut child, Failure::Sink(why));
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
    let said = String::from_utf8_lossy(&err);
    let said = crate::tools::visible(said.trim());
    if status.success() {
        return Ok(said);
    }
    Err(Failure::Exit(status.code().unwrap_or(-1), said))
}

/// A full object id, as git names one: 40 or 64 hexadecimal digits.
pub(crate) fn object_id(id: &str) -> bool {
    (id.len() == 40 || id.len() == 64) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// What an outside git keeps of this process's environment.
pub fn kept_env() -> Vec<(String, OsString)> {
    KEPT.iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (name.to_string(), value)))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
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

    /// The output of git run in `dir`.
    fn said(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A push is staged against the remote branch's tip as it is now,
    /// and sends one commit to one branch from the publish repository,
    /// its hooks off: a fast-forward plain, anything else refused unless
    /// forced, and forced only while the remote still names the id the
    /// lease expects; the remote's refusal is said.
    #[test]
    fn a_push_sends_one_commit_to_one_branch_with_its_lease() {
        if !have_git() {
            return;
        }
        let scratch = Scratch::new("git-push");
        let root = scratch.state().root().to_path_buf();
        let up = root.join("up");
        std::fs::create_dir_all(&up).unwrap();
        upstream(&up, &["init", "--quiet"]);
        std::fs::write(up.join("README"), "one\n").unwrap();
        upstream(&up, &["add", "."]);
        upstream(&up, &["commit", "--quiet", "-m", "one"]);
        // The remote's own hooks run, and what they say is told.
        let told = up.join(".git/hooks/post-receive");
        std::fs::write(&told, "#!/bin/sh\necho the remote took it >&2\n").unwrap();
        std::fs::set_permissions(&told, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let mut worker = Worker::new(&root.join("worker"), &kept_env()).unwrap();
        worker.file = true;
        let remote = local(&up);
        let stores = root.join("store");
        let store = worker.store(&stores, &remote).unwrap();
        worker.fetch(&store, &remote).unwrap();
        let base = worker.resolve(&store, "main").unwrap();
        let work = root.join("work");
        upstream(&root, &["clone", "--quiet", up.to_str().unwrap(), "work"]);
        let packed = |id: &str, name: &str| {
            let mut child = Command::new("git")
                .current_dir(&work)
                .args(["pack-objects", "--stdout", "--revs", "--quiet"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            std::io::Write::write_all(
                child.stdin.as_mut().unwrap(),
                format!("{id}\n^{base}\n").as_bytes(),
            )
            .unwrap();
            drop(child.stdin.take());
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success());
            let pack = root.join(name);
            std::fs::write(&pack, out.stdout).unwrap();
            pack
        };
        let commit = |message: &str| {
            std::fs::write(work.join(message), message).unwrap();
            upstream(&work, &["add", message]);
            upstream(&work, &["commit", "--quiet", "-m", message]);
            said(&work, &["rev-parse", "HEAD"])
        };
        let two = commit("two");
        let publish = root.join("publish/w/r.git");
        let staged = worker
            .stage(
                &stores,
                &publish,
                &remote,
                &packed(&two, "two.pack"),
                &two,
                &base,
                "feature",
            )
            .unwrap();
        assert_eq!(staged.tip, None);
        assert_eq!(staged.evidence.commits, [(two.clone(), "two".to_string())]);
        // The publish repository's own hooks never run.
        let hook = publish.join("hooks/pre-push");
        let ran = root.join("hook-ran");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", ran.display())).unwrap();
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let push = |id: &str, branch: &str, lease: Option<&str>| Push {
            id: id.to_string(),
            branch: branch.to_string(),
            lease: lease.map(str::to_string),
        };
        let answer = worker
            .push(&publish, &remote, &push(&two, "feature", None))
            .unwrap();
        assert!(answer.contains("refs/heads/feature"), "{answer}");
        assert!(answer.contains("the remote took it"), "{answer}");
        assert!(!ran.exists(), "a publish hook ran");
        assert_eq!(said(&up, &["rev-parse", "refs/heads/feature"]), two);
        // A commit off the base, staged against the tip now there.
        upstream(&work, &["checkout", "--quiet", "-b", "other", &base]);
        let three = commit("three");
        let staged = worker
            .stage(
                &stores,
                &publish,
                &remote,
                &packed(&three, "three.pack"),
                &three,
                &base,
                "feature",
            )
            .unwrap();
        assert_eq!(staged.tip.as_deref(), Some(two.as_str()));
        assert_eq!(staged.evidence.merge_base.as_deref(), Some(base.as_str()));
        // Not a fast-forward: refused unless forced.
        let refused = worker
            .push(&publish, &remote, &push(&three, "feature", None))
            .unwrap_err();
        assert!(refused.starts_with("the push was refused"), "{refused}");
        assert!(refused.contains("rejected"), "{refused}");
        // Forced, while the remote names what the lease expects.
        let stale = worker
            .push(&publish, &remote, &push(&three, "feature", Some(&base)))
            .unwrap_err();
        assert!(stale.contains("stale"), "{stale}");
        assert_eq!(said(&up, &["rev-parse", "refs/heads/feature"]), two);
        worker
            .push(&publish, &remote, &push(&three, "feature", Some(&two)))
            .unwrap();
        assert_eq!(said(&up, &["rev-parse", "refs/heads/feature"]), three);
        // A refusal's whole message is said, not a reason's start.
        let refusing = up.join(".git/hooks/pre-receive");
        let long: String = (0..20)
            .map(|n| format!("echo line {n} of the refusal >&2\n"))
            .collect();
        std::fs::write(
            &refusing,
            format!("#!/bin/sh\n{long}echo the end >&2\nexit 1\n"),
        )
        .unwrap();
        std::fs::set_permissions(
            &refusing,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let refused = worker
            .push(&publish, &remote, &push(&three, "elsewhere", None))
            .unwrap_err();
        assert!(refused.contains("the end"), "{refused}");
        std::fs::remove_file(&refusing).unwrap();
        // The remote's own refusal is said: its checked-out branch.
        let checked_out = worker
            .push(&publish, &remote, &push(&three, "main", None))
            .unwrap_err();
        assert!(checked_out.contains("refusing to update"), "{checked_out}");
        // Only a commit's id to a branch, and a lease that is one: the
        // null id would delete the branch.
        let zero = "0".repeat(three.len());
        let tree = said(&work, &["rev-parse", "HEAD^{tree}"]);
        for (id, branch, lease, why) in [
            ("HEAD", "feature", None, "not a full commit id"),
            (&zero, "feature", None, "not a full commit id"),
            (
                three.as_str(),
                "feature",
                Some(zero.as_str()),
                "not a full commit id",
            ),
            (three.as_str(), "-f", None, "not a branch name"),
            (three.as_str(), "+feature", None, "not a branch name"),
            (three.as_str(), ":feature", None, "not a branch name"),
            (three.as_str(), "refs/heads/x", None, "names a ref"),
            (&tree, "feature", None, "not a commit to push"),
            (
                three.as_str(),
                "feature",
                Some("main"),
                "not a full commit id",
            ),
        ] {
            let refused = worker
                .push(&publish, &remote, &push(id, branch, lease))
                .unwrap_err();
            assert!(refused.contains(why), "{id} {branch} {lease:?}: {refused}");
        }
        assert_eq!(said(&up, &["rev-parse", "refs/heads/feature"]), three);
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
        assert_eq!(
            worker.identity().unwrap(),
            crate::repo::Identity {
                name: Some("Human".into()),
                email: Some("h@example.org".into()),
            }
        );
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
        assert_eq!(worker.read_regular(&store, &id, "nothing").unwrap(), None);
        assert_eq!(
            worker.read_regular(&store, &id, ".td-agent").unwrap_err(),
            ".td-agent is not a regular file"
        );
        assert_eq!(
            worker.read(&store, &id, ".td-agent").unwrap(),
            None,
            "a tree"
        );
        // A refresh fetches what upstream has since and resolves each
        // base, one upstream no longer has failing alone.
        upstream(&up, &["commit", "--quiet", "--allow-empty", "-m", "two"]);
        let refreshed = worker
            .refresh(&stores, &remote, &["main".into(), "gone".into()])
            .unwrap();
        let moved = refreshed.first().unwrap().clone().unwrap();
        assert!(object_id(&moved) && moved != id, "{refreshed:?}");
        assert!(refreshed
            .get(1)
            .unwrap()
            .as_ref()
            .unwrap_err()
            .contains("no branch"));
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
    fn each_bases_rules_are_read_once_within_the_answers_bound() {
        use crate::rules::{Read, MAX_CARRIED};
        let ids: Vec<String> = ["a", "b", "a", "c", "d"]
            .iter()
            .map(|c| c.repeat(40))
            .collect();
        let line = format!("deny shell {}\n", "x".repeat(500));
        let mut reads = Vec::new();
        let found = rules_at(&ids, |id| {
            reads.push(id.to_string());
            Ok(match id.chars().next() {
                Some('a') => Some(b"ask shell git push\n".to_vec()),
                Some('b') => None,
                Some('c') => Some(line.repeat(MAX_CARRIED / line.len()).into_bytes()),
                _ => Err("git failed".to_string())?,
            })
        });
        assert_eq!(
            reads,
            [
                ids[0].clone(),
                ids[1].clone(),
                ids[3].clone(),
                ids[4].clone()
            ]
        );
        assert_eq!(found[0], found[2]);
        assert!(matches!(&found[0], Read::Found(rules) if rules.len() == 1));
        assert_eq!(found[1], Read::Absent);
        // The answer's bound counts each commit once: c's would pass it.
        assert!(
            matches!(&found[3], Read::Unread { why } if why.contains("past")),
            "{:?}",
            found[3]
        );
        assert_eq!(found[4], Read::unread("git failed"));
        assert!(found.iter().map(Read::carried).sum::<usize>() <= MAX_CARRIED);
    }

    #[test]
    fn each_bases_instructions_are_read_once_within_the_answers_bound() {
        use crate::repo::Instructions;
        let ids: Vec<String> = ["a", "b", "a", "c", "d", "e"]
            .iter()
            .map(|id| id.repeat(40))
            .collect();
        let mut asked: Vec<String> = Vec::new();
        let big = MAX_INSTRUCTIONS - 5;
        let read = instructions_at(&ids, |id| {
            asked.push(id.to_string());
            match id.get(..1) {
                Some("a") => Ok(Some(("AGENTS.md", b"Run make.\n".to_vec()))),
                Some("b") => Ok(None),
                Some("c") => Ok(Some(("CLAUDE.md", vec![b'x'; big]))),
                Some("d") => Ok(Some(("AGENTS.md", vec![0xff, 0xfe]))),
                _ => Err("git exited 128".into()),
            }
        });
        // A commit asked twice is read once.
        assert_eq!(asked.len(), 5);
        let found = Instructions::Found {
            name: "AGENTS.md".into(),
            text: "Run make.\n".into(),
        };
        assert_eq!(read.first(), Some(&found));
        assert_eq!(read.get(1), Some(&Instructions::Absent));
        assert_eq!(read.get(2), Some(&found));
        // Past the answer's bound with the others: said, not carried.
        assert!(
            matches!(read.get(3), Some(Instructions::Unread { why }) if why.contains("past")),
            "not said past the bound"
        );
        assert_eq!(
            read.get(4),
            Some(&Instructions::Unread {
                why: "AGENTS.md is not UTF-8".into()
            })
        );
        assert_eq!(
            read.get(5),
            Some(&Instructions::Unread {
                why: "git exited 128".into()
            })
        );
        let carried: usize = read.iter().map(Instructions::carried).sum();
        assert!(carried <= MAX_INSTRUCTIONS);
    }

    #[test]
    fn the_store_service_answers_each_ask_for_its_conversation() {
        // A worker that cannot be made answers every ask with why.
        let service = Service::start("relative".into(), "/nowhere".into(), Vec::new());
        let id = crate::store::Id::random().unwrap();
        let remote = Remote::parse("https://example.org/a/td").unwrap();
        service
            .ask(id.clone(), remote.clone(), vec!["main".into()])
            .unwrap();
        service
            .ask(id.clone(), remote, vec!["next".into()])
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut answers = Vec::new();
        while answers.len() < 2 {
            assert!(Instant::now() < deadline, "no answer came");
            answers.extend(service.answers());
            std::thread::sleep(Duration::from_millis(10));
        }
        for done in answers {
            let Done::Prepared {
                conversation,
                remote,
                result,
                ..
            } = done
            else {
                panic!("not a preparation's answer");
            };
            assert_eq!(conversation, id);
            assert_eq!(remote, "https://example.org/a/td");
            let why = result.unwrap_err();
            assert!(why.starts_with("the git worker"), "{why}");
        }
        // The window's refresh is answered for the window, with its bases.
        service
            .refresh(
                Remote::parse("https://example.org/a/td").unwrap(),
                vec!["main".into()],
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "no answer came");
            if let Some(done) = service.answers().pop() {
                let Done::Refreshed {
                    remote,
                    bases,
                    result,
                    asker,
                } = done
                else {
                    panic!("not a refresh's answer");
                };
                assert_eq!(
                    (remote.as_str(), bases),
                    ("https://example.org/a/td", vec!["main".to_string()])
                );
                assert!(result.unwrap_err().starts_with("the git worker"));
                assert_eq!(asker, None);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // A `git_fetch`'s names its conversation and call.
        let id = crate::store::Id::random().unwrap();
        service
            .fetch_now(
                id.clone(),
                7,
                Remote::parse("https://example.org/a/td").unwrap(),
                vec!["main".into()],
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "no answer came");
            if let Some(done) = service.answers().pop() {
                let Done::Refreshed { asker, .. } = done else {
                    panic!("not a refresh's answer");
                };
                assert_eq!(asker, Some((id, 7)));
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A conversation's ask, a preparation or a `git_fetch`, goes before
    /// any background fetch queued ahead of it; each kind keeps its order.
    #[test]
    fn a_preparation_goes_before_queued_background_fetches() {
        let remote = |path: &str| Remote::parse(&format!("https://example.org/{path}")).unwrap();
        let id = crate::store::Id::random().unwrap();
        let refresh = |path: &str, asked: bool| Job::Refresh {
            remote: remote(path),
            bases: vec!["main".into()],
            asker: asked.then(|| (id.clone(), 1)),
        };
        let mut queue: std::collections::VecDeque<Job> = [
            refresh("a", false),
            refresh("b", false),
            Job::Prepare {
                conversation: id.clone(),
                remote: remote("c"),
                bases: vec!["main".into()],
            },
            refresh("d", true),
            Job::Prepare {
                conversation: id.clone(),
                remote: remote("e"),
                bases: vec!["main".into()],
            },
        ]
        .into();
        let order: Vec<String> = std::iter::from_fn(|| next(&mut queue))
            .map(|job| match job {
                Job::Prepare { remote, .. } => format!("prepare {}", remote.url()),
                Job::Refresh {
                    remote,
                    asker: Some(_),
                    ..
                } => format!("fetch {}", remote.url()),
                Job::Refresh { remote, .. } => format!("refresh {}", remote.url()),
            })
            .collect();
        assert_eq!(
            order,
            [
                "prepare https://example.org/c",
                "fetch https://example.org/d",
                "prepare https://example.org/e",
                "refresh https://example.org/a",
                "refresh https://example.org/b"
            ]
        );
    }

    /// A line longer than the scan keeps is scanned in pieces that
    /// overlap, so a shape across where one is cut is still found, in
    /// an added line; in a removed one it is not. A piece's end decides
    /// nothing a longer line would undo, and the byte before what a
    /// later piece scans is still context.
    #[test]
    fn a_long_lines_shape_is_found_across_its_pieces() {
        let github = format!("ghp_{}", "a".repeat(36));
        let aws = |n: usize| format!("AKIA{}", "B".repeat(n));
        for (at, token, sign, found) in [
            (SCAN_LINE - 10, github.clone(), b'+', true),
            (3 * SCAN_LINE + 7, github.clone(), b'+', true),
            (SCAN_LINE - 10, github.clone(), b'-', false),
            // Cut when 16 of its 17 letters are in: too long, all told.
            (SCAN_LINE - 21, aws(17), b'+', false),
            (SCAN_LINE - 21, aws(16), b'+', true),
            // The tail of a word, just inside what the next piece keeps.
            (
                SCAN_LINE - OVERLAP - 2,
                format!("x{}", aws(16)),
                b'+',
                false,
            ),
        ] {
            let mut line = vec![sign];
            line.extend(std::iter::repeat_n(b' ', at));
            line.extend_from_slice(token.as_bytes());
            line.extend(std::iter::repeat_n(b' ', 1000));
            line.push(b'\n');
            let mut scanner = Scanner::new(Mode::Diff, Vec::new(), 0);
            let mut text = b"\0commit c\n+++ b/f\n@@ -1,1 +1,1 @@\n".to_vec();
            if sign == b'+' {
                text.extend_from_slice(b"- old\n");
            } else {
                text.extend_from_slice(b"+ new\n");
            }
            text.extend_from_slice(&line);
            // In pieces of any size.
            for piece in text.chunks(4093) {
                scanner.take(piece);
            }
            scanner.finish();
            assert_eq!(!scanner.found.is_empty(), found, "{at} {token} {sign}");
            if found {
                assert_eq!(scanner.found[0].path.as_deref(), Some("f"));
            }
        }
    }

    /// A hunk's header says how many of the file's lines follow it.
    #[test]
    fn a_hunks_header_counts_its_lines() {
        assert_eq!(hunk(b"@@ -12,3 +12,4 @@ fn main()"), Some((3, 4)));
        assert_eq!(hunk(b"@@ -1 +1 @@"), Some((1, 1)));
        assert_eq!(hunk(b"@@ -0,0 +1,2 @@"), Some((0, 2)));
        for line in [&b"+++ b/x"[..], b"@@ -a,1 +1 @@", b"@@ -1,1 +1,1", b""] {
            assert_eq!(hunk(line), None, "{line:?}");
        }
    }

    /// A sink slower than git's grace still takes all git said, though
    /// git exits long before it does.
    #[test]
    fn a_slow_sink_takes_all_git_said() {
        if Command::new("sh").arg("-c").arg("true").status().is_err() {
            eprintln!("skipped: no sh on PATH");
            return;
        }
        let mut taken = 0usize;
        run_into(
            Command::new("sh").args(["-c", "head -c 300000 /dev/zero"]),
            None,
            1 << 20,
            LOCAL_TIME,
            &mut |bytes| {
                taken += bytes.len();
                std::thread::sleep(Duration::from_millis(50));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(taken, 300_000);
    }

    /// A helper left behind that keeps talking on standard error holds a
    /// run no longer than the grace, as one that keeps quiet does.
    #[test]
    fn a_helper_chattering_on_its_error_holds_no_run() {
        if Command::new("sh").arg("-c").arg("true").status().is_err() {
            eprintln!("skipped: no sh on PATH");
            return;
        }
        let started = Instant::now();
        let said = run(
            Command::new("sh").args([
                "-c",
                "(for i in 1 2 3 4 5 6 7 8 9 10; do echo x >&2; sleep 0.3; done &); echo hi",
            ]),
            64,
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(said, b"hi\n");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    /// A sink that refuses what git says ends it, said as the sink's.
    #[test]
    fn a_runs_sink_takes_its_output_as_it_comes_or_ends_it() {
        if Command::new("sh").arg("-c").arg("true").status().is_err() {
            eprintln!("skipped: no sh on PATH");
            return;
        }
        let mut taken = Vec::new();
        run_into(
            Command::new("sh").args(["-c", "echo one; echo two"]),
            None,
            64,
            LOCAL_TIME,
            &mut |bytes| {
                taken.extend_from_slice(bytes);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(taken, b"one\ntwo\n");
        let refused = run_into(
            Command::new("sh").args(["-c", "echo one"]),
            None,
            64,
            LOCAL_TIME,
            &mut |_| Err("full".into()),
        )
        .unwrap_err();
        assert_eq!(refused.to_string(), "its output: full");
        // Past its limit, the sink is not handed the rest.
        let mut taken = Vec::new();
        let long = run_into(
            Command::new("sh").args(["-c", "head -c 100000 /dev/zero"]),
            None,
            10,
            LOCAL_TIME,
            &mut |bytes| {
                taken.extend_from_slice(bytes);
                Ok(())
            },
        );
        assert!(matches!(long, Err(Failure::TooLong)));
        assert!(taken.len() <= 10, "{}", taken.len());
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
