//! The client's command-line options and `ssh_config` subset: the keywords td's
//! callers pass and a person at a terminal uses (DESIGN.md section 6). Any other is
//! an error, because a config file that asks for a removed feature must not silently
//! connect some other way.

use crate::error::{Error, Result};
use crate::kex::{CIPHER, DEFAULT_KEX, HOST_KEY_ALGORITHM};
use crate::pattern::glob;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const MAX_CONFIG_FILE: u64 = 1024 * 1024;
/// Rekeying each packet would hold up every byte behind a host-key signature.
const MIN_REKEY_LIMIT: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strict {
    Yes,
    AcceptNew,
    No,
    Ask,
}

/// `RequestTTY`, which `-t` and `-T` override.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestTty {
    No,
    Yes,
    Force,
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Quiet,
    Fatal,
    Error,
    Info,
    Verbose,
    Debug,
}

/// First obtained value wins, so every field starts unset.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<String>,
    pub batch_mode: Option<bool>,
    pub strict: Option<Strict>,
    pub user_known_hosts: Option<Vec<String>>,
    pub global_known_hosts: Option<Vec<String>>,
    pub host_key_alias: Option<String>,
    pub hash_known_hosts: Option<bool>,
    pub connect_timeout: Option<u64>,
    pub server_alive_interval: Option<u64>,
    pub server_alive_count_max: Option<u32>,
    pub rekey_limit: Option<u64>,
    pub send_env: Vec<String>,
    pub log_level: Option<LogLevel>,
    pub request_tty: Option<RequestTty>,
    pub kex: Option<Vec<&'static str>>,
    preferred_ok: Option<bool>,
    ignore_unknown: Vec<String>,
    /// Which of `STATED` have been given: as everywhere, the first value decides.
    stated: Vec<&'static str>,
    /// The first forwarding asked for, with where: there is none to give, which is
    /// what `ClearAllForwardings yes` asks for whichever line it stands on.
    forwarding: Option<String>,
    /// Algorithm keywords that came up empty, reported once validated: the
    /// first obtained value decides, as it does for everything else.
    empty_algorithms: Vec<&'static str>,
    host_key_set: bool,
    cipher_set: bool,
    pubkey_set: bool,
}

/// Keywords td's callers write only to state what td-ssh already does, and the one
/// value that states it. `td-remote` passes the first three for a stock `ssh`'s sake
/// (`-a` stands for `ForwardAgent no`), and the rest are in the file `td-vm-guest`
/// wrote into a VM's state and compares byte for byte on every start, so an older
/// VM's file must keep reading.
const STATED: &[(&str, &str)] = &[
    ("ClearAllForwardings", "yes"),
    ("RemoteCommand", "none"),
    ("ControlPath", "none"),
    ("IdentityAgent", "none"),
    ("CheckHostIP", "no"),
    ("UpdateHostKeys", "no"),
    ("ForwardAgent", "no"),
    ("PermitLocalCommand", "no"),
    ("CanonicalizeHostname", "no"),
    ("ProxyCommand", "none"),
    ("ProxyJump", "none"),
];

/// What `ClearAllForwardings yes` discards. Any of them without it is refused.
const FORWARDINGS: &[&str] = &["localforward", "remoteforward", "dynamicforward"];

/// Keywords whose silent absence would trust another host key, leave from another
/// address or run something other than asked, so that `IgnoreUnknown` naming one
/// still refuses it.
const NOT_WAIVED: &[&str] = &[
    "addressfamily",
    "bindaddress",
    "bindinterface",
    "certificatefile",
    "forkafterauthentication",
    "knownhostscommand",
    "pkcs11provider",
    "revokedhostkeys",
    "securitykeyprovider",
    "sessiontype",
    "setenv",
    "stdinnull",
    "tunnel",
    "tunneldevice",
];

/// Who a configuration file must be owned by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// `~/.ssh/config`: checked as OpenSSH checks it.
    User,
    /// `-F FILE`, the caller's own choice, or the system's file: unchecked.
    Given,
}

/// The real uid, from `/proc`, since std offers no `getuid` and td-ssh takes no
/// new syscall for one. Failing closed: every caller is a permission check.
pub fn caller_uid() -> Result<u32> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|error| Error::refused(format!("reading /proc/self/status: {error}")))?;
    uid_from_status(&status).ok_or_else(|| Error::refused("no Uid line in /proc/self/status"))
}

fn uid_from_status(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_ascii_whitespace()
        .next()?
        .parse()
        .ok()
}

/// As OpenSSH's per-user configuration check: nobody else may have written it.
fn safe_config_owner(uid: u32, mode: u32, caller: u32) -> bool {
    (uid == caller || uid == 0) && mode & 0o022 == 0
}

fn parse_bool(keyword: &str, value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "yes" | "true" | "on" => Ok(true),
        "no" | "false" | "off" => Ok(false),
        _ => Err(Error::refused(format!(
            "{keyword}: expected yes or no, not {value}"
        ))),
    }
}

fn parse_number<T: std::str::FromStr>(keyword: &str, value: &str) -> Result<T> {
    value
        .parse()
        .map_err(|_| Error::refused(format!("{keyword}: bad number {value}")))
}

/// `ConnectTimeout`-style seconds. Only plain numbers: time suffixes are not a
/// form any td caller writes, and refusing beats misreading.
fn parse_seconds(keyword: &str, value: &str) -> Result<u64> {
    parse_number(keyword, value)
}

/// `RekeyLimit BYTES[K|M|G] [none]`, where `default` keeps the compiled limit. A
/// time limit is refused: dropping it silently would weaken what was asked for.
fn parse_rekey_limit(value: &str) -> Result<u64> {
    let mut parts = value.split_ascii_whitespace();
    let bytes = parts.next().unwrap_or_default();
    match (parts.next(), parts.next()) {
        (None | Some("none"), None) => {}
        _ => {
            return Err(Error::refused(format!(
                "RekeyLimit {value}: td-ssh has no time limit"
            )))
        }
    }
    if bytes == "default" {
        return Ok(u64::MAX);
    }
    let (digits, scale) = match bytes.as_bytes().last() {
        Some(b'K' | b'k') => (bytes.get(..bytes.len() - 1), 1u64 << 10),
        Some(b'M' | b'm') => (bytes.get(..bytes.len() - 1), 1 << 20),
        Some(b'G' | b'g') => (bytes.get(..bytes.len() - 1), 1 << 30),
        _ => (Some(bytes), 1),
    };
    let number: u64 = parse_number("RekeyLimit", digits.unwrap_or_default())?;
    number
        .checked_mul(scale)
        .filter(|limit| *limit >= MIN_REKEY_LIMIT)
        .ok_or_else(|| Error::refused(format!("RekeyLimit {value}: out of range")))
}

fn set<T>(slot: &mut Option<T>, value: impl FnOnce() -> Result<T>) -> Result<()> {
    if slot.is_none() {
        *slot = Some(value()?);
    }
    Ok(())
}

/// `+`/`^` keep the compiled set, `-` subtracts, a plain list intersects.
/// Returns the compiled names kept, in the compiled order.
fn filter_algorithms(value: &str, compiled: &[&'static str]) -> Vec<&'static str> {
    if value.starts_with('+') || value.starts_with('^') {
        return compiled.to_vec();
    }
    if let Some(removed) = value.strip_prefix('-') {
        return compiled
            .iter()
            .filter(|name| !removed.split(',').any(|pattern| glob(pattern, name)))
            .copied()
            .collect();
    }
    compiled
        .iter()
        .filter(|name| value.split(',').any(|pattern| glob(pattern, name)))
        .copied()
        .collect()
}

/// Splits `Keyword value`, `Keyword=value` and a quoted value. Returns the
/// keyword and the remaining text, with a `#` outside quotes ending the line.
fn split_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let end = line.find([' ', '\t', '=']).unwrap_or(line.len());
    let (keyword, rest) = line.split_at(end);
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest).trim();
    Some((keyword.to_ascii_lowercase(), strip_comment(rest)))
}

fn strip_comment(value: &str) -> String {
    let mut in_quotes = false;
    let mut previous_space = true;
    for (index, c) in value.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            '#' if !in_quotes && previous_space => {
                return value.get(..index).unwrap_or(value).trim_end().to_string()
            }
            _ => {}
        }
        previous_space = c == ' ' || c == '\t';
    }
    value.to_string()
}

/// Whitespace-separated words, with double quotes grouping.
pub fn words(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut started = false;
    for c in value.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                started = true;
            }
            ' ' | '\t' if !in_quotes => {
                if started {
                    out.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            _ => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(current);
    }
    out
}

fn first_word(value: &str) -> String {
    words(value).into_iter().next().unwrap_or_default()
}

impl Options {
    /// Applies a command-line `-o Keyword=value` or `Keyword value`.
    pub fn apply_option(&mut self, text: &str) -> Result<()> {
        let (keyword, value) = split_line(text)
            .ok_or_else(|| Error::refused(format!("bad configuration option: {text}")))?;
        match keyword.as_str() {
            "host" | "match" | "include" => Err(Error::refused(format!(
                "{keyword} is not valid on the command line"
            ))),
            _ => self.apply_keyword(&keyword, &value),
        }
    }

    /// One keyword from any source. See the classes in DESIGN.md section 6.
    pub fn apply_keyword(&mut self, keyword: &str, value: &str) -> Result<()> {
        let word = first_word(value);
        match keyword {
            "hostname" => set(&mut self.hostname, || Ok(word.clone())),
            "user" => set(&mut self.user, || Ok(word.clone())),
            "port" => set(&mut self.port, || {
                parse_port(&word).ok_or_else(|| Error::refused(format!("Port: bad port {word}")))
            }),
            "identityfile" => {
                self.identity_files.push(word);
                Ok(())
            }
            // How td-ssh behaves either way, with no agent to prefer.
            "identitiesonly" => parse_bool("IdentitiesOnly", &word).map(drop),
            "batchmode" => set(&mut self.batch_mode, || parse_bool("BatchMode", &word)),
            "stricthostkeychecking" => set(&mut self.strict, || {
                match word.to_ascii_lowercase().as_str() {
                    "yes" | "true" | "on" => Ok(Strict::Yes),
                    "accept-new" => Ok(Strict::AcceptNew),
                    "no" | "false" | "off" => Ok(Strict::No),
                    "ask" => Ok(Strict::Ask),
                    _ => Err(Error::refused(format!(
                        "StrictHostKeyChecking: bad value {word}"
                    ))),
                }
            }),
            "userknownhostsfile" => set(&mut self.user_known_hosts, || Ok(words(value))),
            "globalknownhostsfile" => set(&mut self.global_known_hosts, || Ok(words(value))),
            "hostkeyalias" => set(&mut self.host_key_alias, || Ok(word.clone())),
            "hashknownhosts" => set(&mut self.hash_known_hosts, || {
                parse_bool("HashKnownHosts", &word)
            }),
            "connecttimeout" => set(&mut self.connect_timeout, || {
                if word.eq_ignore_ascii_case("none") {
                    Ok(0)
                } else {
                    parse_seconds("ConnectTimeout", &word)
                }
            }),
            "rekeylimit" => set(&mut self.rekey_limit, || parse_rekey_limit(value)),
            "serveraliveinterval" => set(&mut self.server_alive_interval, || {
                parse_seconds("ServerAliveInterval", &word)
            }),
            "serveralivecountmax" => set(&mut self.server_alive_count_max, || {
                parse_number("ServerAliveCountMax", &word)
            }),
            "sendenv" => {
                self.send_env.extend(words(value));
                Ok(())
            }
            "loglevel" => set(&mut self.log_level, || parse_log_level(&word)),
            "requesttty" => set(&mut self.request_tty, || {
                match word.to_ascii_lowercase().as_str() {
                    "no" | "false" => Ok(RequestTty::No),
                    "yes" | "true" => Ok(RequestTty::Yes),
                    "force" => Ok(RequestTty::Force),
                    "auto" => Ok(RequestTty::Auto),
                    _ => Err(Error::refused(format!("RequestTTY: bad value {word}"))),
                }
            }),
            "preferredauthentications" => set(&mut self.preferred_ok, || {
                Ok(word
                    .split(',')
                    .any(|method| method.eq_ignore_ascii_case("publickey")))
            }),
            // Algorithm lists.
            "kexalgorithms" => {
                let first = self.kex.is_none();
                if first {
                    let kept = filter_algorithms(&word, DEFAULT_KEX);
                    if kept.is_empty() {
                        self.empty_algorithms.push("KexAlgorithms");
                    }
                    self.kex = Some(kept);
                }
                Ok(())
            }
            "hostkeyalgorithms" => {
                self.one_algorithm(&word, HOST_KEY_ALGORITHM, "HostKeyAlgorithms", |o| {
                    &mut o.host_key_set
                });
                Ok(())
            }
            "pubkeyacceptedalgorithms" => {
                self.one_algorithm(&word, HOST_KEY_ALGORITHM, "PubkeyAcceptedAlgorithms", |o| {
                    &mut o.pubkey_set
                });
                Ok(())
            }
            "ciphers" => {
                self.one_algorithm(&word, CIPHER, "Ciphers", |o| &mut o.cipher_set);
                Ok(())
            }
            "ignoreunknown" => {
                if words(value).len() > 1 {
                    return Err(Error::refused(format!(
                        "IgnoreUnknown {value}: one comma-separated list, with no spaces"
                    )));
                }
                for name in word.split(',') {
                    if name.contains(['*', '?', '!']) {
                        return Err(Error::refused(format!(
                            "IgnoreUnknown {name}: takes keywords, not patterns"
                        )));
                    }
                    self.ignore_unknown.push(name.to_ascii_lowercase());
                }
                Ok(())
            }
            other if FORWARDINGS.contains(&other) => {
                if self.forwarding.is_none() {
                    self.forwarding = Some(format!("{other} {value}"));
                }
                Ok(())
            }
            other => match STATED
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(other))
            {
                Some((name, _)) if self.stated.contains(name) => Ok(()),
                Some((name, stated)) if states(stated, &word) => {
                    self.stated.push(name);
                    Ok(())
                }
                Some((name, _)) => Err(Error::refused(format!(
                    "{name} {value} is not supported by td-ssh"
                ))),
                None if NOT_WAIVED.contains(&other) => Err(Error::refused(format!(
                    "Bad configuration option: {other}, which IgnoreUnknown does not waive"
                ))),
                None if self.ignore_unknown.iter().any(|name| name == other) => Ok(()),
                None => Err(Error::refused(format!("Bad configuration option: {other}"))),
            },
        }
    }

    fn one_algorithm(
        &mut self,
        value: &str,
        compiled: &'static str,
        name: &'static str,
        flag: fn(&mut Self) -> &mut bool,
    ) {
        if !*flag(self) {
            *flag(self) = true;
            if filter_algorithms(value, &[compiled]).is_empty() {
                self.empty_algorithms.push(name);
            }
        }
    }

    /// Judges the effective values once every source has been read.
    pub fn validate(&self) -> Result<()> {
        if let Some(name) = self.empty_algorithms.first() {
            return Err(Error::refused(format!(
                "{name}: no algorithm td-ssh supports remains"
            )));
        }
        if self.preferred_ok == Some(false) {
            return Err(Error::refused("td-ssh authenticates only with public keys"));
        }
        if let Some(forwarding) = &self.forwarding {
            if !self.stated.contains(&"ClearAllForwardings") {
                return Err(Error::refused(format!(
                    "{forwarding}: td-ssh forwards nothing, which only ClearAllForwardings yes \
                     says"
                )));
            }
        }
        Ok(())
    }

    /// Reads one configuration file, following `Host`.
    pub fn read_file(&mut self, path: &Path, host: &str, origin: Origin) -> Result<()> {
        let cannot_open =
            |error| Error::refused(format!("Can't open config {}: {error}", path.display()));
        let file = std::fs::File::open(path).map_err(cannot_open)?;
        let metadata = file.metadata().map_err(cannot_open)?;
        if origin == Origin::User
            && !safe_config_owner(metadata.uid(), metadata.mode(), caller_uid()?)
        {
            return Err(Error::refused(format!(
                "Bad owner or permissions on {}",
                path.display()
            )));
        }
        if metadata.len() > MAX_CONFIG_FILE {
            return Err(Error::refused(format!("{} is too large", path.display())));
        }
        let mut text = String::new();
        (&file).read_to_string(&mut text).map_err(cannot_open)?;
        self.read_text(&text, host, &path.display().to_string())
    }

    /// Exposed so tests read text without touching the filesystem.
    pub fn read_text(&mut self, text: &str, host: &str, source: &str) -> Result<()> {
        let mut active = true;
        for (index, line) in text.lines().enumerate() {
            let Some((keyword, value)) = split_line(line) else {
                continue;
            };
            let located =
                |error: Error| Error::refused(format!("{source} line {}: {error}", index + 1));
            match keyword.as_str() {
                "host" => active = host_matches(&value, host),
                // Each changes what the lines around it mean, so `IgnoreUnknown` and an
                // inactive `Host` block do not reach them: skipping one would apply
                // what followed it to the wrong host, or lose what it held.
                "match" | "include" => {
                    return Err(located(Error::refused(format!(
                        "{keyword} is not supported by td-ssh"
                    ))))
                }
                _ if active => self.apply_keyword(&keyword, &value).map_err(located)?,
                _ => {}
            }
        }
        Ok(())
    }

    pub fn effective_host_alias<'a>(&'a self, host: &'a str) -> &'a str {
        self.host_key_alias.as_deref().unwrap_or(host)
    }
}

/// Whether `word` is the stated value, in any form OpenSSH reads it in.
fn states(stated: &str, word: &str) -> bool {
    match stated {
        "yes" => matches!(word.to_ascii_lowercase().as_str(), "yes" | "true"),
        "no" => matches!(word.to_ascii_lowercase().as_str(), "no" | "false"),
        _ => word.eq_ignore_ascii_case(stated),
    }
}

/// A port a connection can be made to.
pub fn parse_port(value: &str) -> Option<u16> {
    value.parse().ok().filter(|port| *port != 0)
}

fn parse_log_level(word: &str) -> Result<LogLevel> {
    match word.to_ascii_uppercase().as_str() {
        "QUIET" => Ok(LogLevel::Quiet),
        "FATAL" => Ok(LogLevel::Fatal),
        "ERROR" => Ok(LogLevel::Error),
        "INFO" => Ok(LogLevel::Info),
        "VERBOSE" => Ok(LogLevel::Verbose),
        "DEBUG" | "DEBUG1" | "DEBUG2" | "DEBUG3" => Ok(LogLevel::Debug),
        _ => Err(Error::refused(format!("LogLevel: bad value {word}"))),
    }
}

/// `Host` patterns, each with its own `!` and any match needing a positive one.
/// Case-insensitive, as `Match host` and `known_hosts` are.
fn host_matches(value: &str, host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let mut accepted = false;
    for pattern in words(value) {
        let pattern = pattern.to_ascii_lowercase();
        match pattern.strip_prefix('!') {
            Some(rest) => {
                if glob(rest, &host) {
                    return false;
                }
            }
            None => {
                if glob(&pattern, &host) {
                    accepted = true;
                }
            }
        }
    }
    accepted
}

/// `~` and the `%` tokens OpenSSH defines for path-valued options. An unknown
/// token refuses rather than opening a literally named file.
pub fn expand_path(
    value: &str,
    home: &Path,
    host: &str,
    original_host: &str,
    remote_user: &str,
    local_user: &str,
    port: u16,
) -> Result<PathBuf> {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('d') => out.push_str(&home.display().to_string()),
            Some('h') => out.push_str(host),
            Some('n') => out.push_str(original_host),
            Some('r') => out.push_str(remote_user),
            Some('u') => out.push_str(local_user),
            Some('p') => out.push_str(&port.to_string()),
            other => {
                return Err(Error::refused(format!(
                    "unknown path token %{}",
                    other.map(String::from).unwrap_or_default()
                )))
            }
        }
    }
    Ok(match out.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if out == "~" => home.to_path_buf(),
        None => PathBuf::from(out),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn rekey_limit_takes_bytes_and_refuses_a_time() {
        assert_eq!(parse_rekey_limit("2M").unwrap(), 2 << 20);
        assert_eq!(parse_rekey_limit("1g none").unwrap(), 1 << 30);
        assert_eq!(parse_rekey_limit("100000").unwrap(), 100_000);
        assert_eq!(parse_rekey_limit("default").unwrap(), u64::MAX);
        for bad in [
            "1h",
            "2M 1h",
            "default 30m",
            "",
            "1K",
            "x",
            "99999999999999999G",
        ] {
            assert!(parse_rekey_limit(bad).is_err(), "{bad}");
        }
    }

    fn read(text: &str, host: &str) -> Result<Options> {
        let mut options = Options::default();
        options.read_text(text, host, "test")?;
        Ok(options)
    }

    #[test]
    fn host_patterns_ignore_case() {
        let text = "Host Example.* !SKIP.example\n  Port 7\n";
        assert_eq!(read(text, "example.org").unwrap().port, Some(7));
        assert_eq!(read(text, "EXAMPLE.org").unwrap().port, Some(7));
        assert_eq!(read(text, "skip.example").unwrap().port, None);
        assert_eq!(read(text, "Skip.Example").unwrap().port, None);
    }

    fn write_mode(path: &Path, text: &str, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn the_owner_rule_is_openssh_s() {
        assert!(safe_config_owner(1000, 0o644, 1000));
        assert!(safe_config_owner(0, 0o644, 1000));
        assert!(!safe_config_owner(1001, 0o600, 1000));
        assert!(!safe_config_owner(1000, 0o664, 1000));
        assert!(!safe_config_owner(0, 0o646, 1000));
        assert_eq!(uid_from_status("Name:\tx\nUid:\t7\t8\t9\t10\n"), Some(7));
        assert_eq!(uid_from_status("Name:\tx\n"), None);
    }

    #[test]
    fn a_user_config_writable_by_others_is_refused_with_openssh_s_text() {
        let dir = td_fs::TempDir::new("td-ssh-config").unwrap();
        let path = dir.path().join("config");
        let mut options = Options::default();
        write_mode(&path, "Port 1\n", 0o600);
        options.read_file(&path, "h", Origin::User).unwrap();
        assert_eq!(options.port, Some(1));
        write_mode(&path, "Port 2\n", 0o664);
        let error = Options::default()
            .read_file(&path, "h", Origin::User)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            error,
            format!("Bad owner or permissions on {}", path.display())
        );
        // `-F` and the system file are not held to it.
        let mut options = Options::default();
        options.read_file(&path, "h", Origin::Given).unwrap();
        assert_eq!(options.port, Some(2));
    }

    #[test]
    fn the_caller_is_who_owns_what_it_creates() {
        let dir = td_fs::TempDir::new("td-ssh-config").unwrap();
        let path = dir.path().join("mine");
        std::fs::write(&path, "").unwrap();
        let owner = std::fs::metadata(&path).unwrap().uid();
        assert_eq!(caller_uid().unwrap(), owner);
    }

    #[test]
    fn the_first_obtained_value_wins_across_host_blocks() {
        let text = "Host a*\n  Port 1\nHost *\n  Port 2\n  User global\n";
        let options = read(text, "abc").unwrap();
        assert_eq!(options.port, Some(1));
        assert_eq!(options.user.as_deref(), Some("global"));
        assert_eq!(read(text, "xyz").unwrap().port, Some(2));
    }

    #[test]
    fn keyword_forms_and_comments_parse() {
        let options = read(
            "PORT=22\nUser \"quoted name\" # trailing\nHostName h#ash\n# whole line\n\n",
            "h",
        )
        .unwrap();
        assert_eq!(options.port, Some(22));
        assert_eq!(options.user.as_deref(), Some("quoted name"));
        assert_eq!(options.hostname.as_deref(), Some("h#ash"));
    }

    #[test]
    fn negated_host_patterns_exclude() {
        let text = "Host * !skip\n  Port 7\n";
        assert_eq!(read(text, "other").unwrap().port, Some(7));
        assert_eq!(read(text, "skip").unwrap().port, None);
    }

    #[test]
    fn every_option_td_callers_pass_is_accepted() {
        // bootsuccess, td-agent, td-remote, td-vm-guest's generated file and git.
        let mut options = Options::default();
        for option in [
            "BatchMode=yes",
            "IdentitiesOnly=yes",
            "PreferredAuthentications=publickey",
            "StrictHostKeyChecking=yes",
            "UserKnownHostsFile=/run/x",
            "GlobalKnownHostsFile=/dev/null",
            "CheckHostIP=no",
            "UpdateHostKeys=no",
            "ForwardAgent=no",
            "ClearAllForwardings=yes",
            "PermitLocalCommand=no",
            "CanonicalizeHostname=no",
            "ProxyCommand=none",
            "ProxyJump=none",
            "RemoteCommand=none",
            "ControlPath=none",
            "ConnectTimeout=30",
            "ServerAliveInterval=15",
            "ServerAliveCountMax=4",
            "KexAlgorithms=mlkem768x25519-sha256,sntrup761x25519-sha512,curve25519-sha256",
            "HostKeyAlgorithms=ssh-ed25519",
            "PubkeyAcceptedAlgorithms=ssh-ed25519",
            "Ciphers=chacha20-poly1305@openssh.com",
            "LogLevel=INFO",
            "HashKnownHosts=yes",
            "IdentityAgent=none",
            "HostKeyAlias=td-host",
            "SendEnv=GIT_PROTOCOL",
        ] {
            options.apply_option(option).unwrap();
        }
        options.validate().unwrap();
        assert_eq!(options.kex.as_deref(), Some(DEFAULT_KEX));
        assert_eq!(options.send_env, ["GIT_PROTOCOL"]);
    }

    /// td-vm-guest compares this file with the one a VM's state holds and refuses a
    /// difference, so an existing VM reads this text for as long as it lives. A copy of
    /// `td-vm-guest/src/workspace.rs`'s `ssh_config`, which names this test.
    #[test]
    fn a_vm_s_generated_file_still_reads() {
        let text = "Host td-host\n  HostName 10.0.2.2\n  User u\n  Port 22\n  HostKeyAlias td-host\n  IdentityFile /s/git/id_ed25519\n  IdentitiesOnly yes\n  IdentityAgent none\n  BatchMode yes\n  PreferredAuthentications publickey\n  StrictHostKeyChecking yes\n  UserKnownHostsFile /s/ssh/known_hosts\n  GlobalKnownHostsFile /dev/null\n  CheckHostIP no\n  UpdateHostKeys no\n  ForwardAgent no\n  ClearAllForwardings yes\n  PermitLocalCommand no\n  CanonicalizeHostname no\n  ProxyCommand none\n  ProxyJump none\n  ConnectTimeout 10\n  ServerAliveInterval 15\n  ServerAliveCountMax 3\n";
        let options = read(text, "td-host").unwrap();
        options.validate().unwrap();
        assert_eq!(options.hostname.as_deref(), Some("10.0.2.2"));
    }

    #[test]
    fn a_stated_keyword_takes_only_the_value_that_states_what_td_ssh_does() {
        for text in [
            "ProxyCommand nc %h %p",
            "ProxyJump bastion",
            "RemoteCommand tmux",
            "ControlPath /tmp/x",
            "PermitLocalCommand yes",
            "CanonicalizeHostname always",
            "ForwardAgent yes",
            "IdentityAgent /run/agent",
            "ClearAllForwardings no",
            // Not OpenSSH's no: it reads this as an agent socket named `off`.
            "ForwardAgent off",
        ] {
            let error = read(text, "h").err().unwrap().to_string();
            assert!(error.contains("not supported"), "{text}: {error}");
        }
        let error = read("ForwardAgent yes", "h").err().unwrap().to_string();
        assert!(error.contains("ForwardAgent yes"), "{error}");
        // OpenSSH's other spellings of the same value.
        for text in [
            "ProxyCommand \"none\"",
            "ProxyCommand NONE",
            "ClearAllForwardings true",
            "ForwardAgent false",
            "CheckHostIP=no # trailing",
        ] {
            read(text, "h").unwrap();
        }
        // No `IgnoreUnknown` waives one: it is not unknown.
        assert!(read("IgnoreUnknown ProxyCommand\nProxyCommand x\n", "h").is_err());
        // td-remote's overrides beat the same keyword in a config file.
        let mut options = Options::default();
        options.apply_option("RemoteCommand=none").unwrap();
        options
            .read_text("RemoteCommand tmux\n", "h", "test")
            .unwrap();
        // And the other way about, the first still decides.
        assert!(read("RemoteCommand tmux\nRemoteCommand none\n", "h").is_err());
    }

    #[test]
    fn a_keyword_td_ssh_has_no_use_for_is_an_error() {
        for text in [
            // Removed features, whatever the value.
            "ForwardX11 no",
            "ForwardX11 yes",
            "ForwardX11Trusted yes",
            "ForwardX11Timeout 1h",
            "PasswordAuthentication yes",
            "PasswordAuthentication no",
            "KbdInteractiveAuthentication no",
            "NumberOfPasswordPrompts 1",
            "PubkeyAuthentication yes",
            "PubkeyAcceptedKeyTypes ssh-ed25519",
            "Tunnel yes",
            "BindAddress 10.0.0.1",
            "ControlMaster auto",
            "ControlPersist 10m",
            "Compression yes",
            "AddKeysToAgent yes",
            "SetEnv A=b",
            "CertificateFile x",
            // Conveniences with no caller.
            "ConnectionAttempts 3",
            "EscapeChar none",
            "SessionType none",
            "MACs hmac-sha2-256",
            "Protocol 2",
            "NotAKeyword x",
        ] {
            let error = read(text, "h").err().unwrap().to_string();
            assert!(
                error.contains("Bad configuration option"),
                "{text}: {error}"
            );
        }
    }

    /// td-remote's `-a`, `ClearAllForwardings yes` and `-T` keep what a person's file
    /// asks for out of its link: with them the file reads, and without them it refuses.
    #[test]
    fn td_remote_s_options_still_override_a_person_s_file() {
        let person = "Host *\n  ForwardAgent yes\n  LocalForward 8080 127.0.0.1:80\n  RemoteForward 9 h:9\n  DynamicForward 1080\n  RequestTTY force\n";
        let mut options = Options::default();
        for option in ["ForwardAgent=no", "ClearAllForwardings=yes"] {
            options.apply_option(option).unwrap();
        }
        options.read_text(person, "h", "test").unwrap();
        options.validate().unwrap();
        assert_eq!(options.request_tty, Some(RequestTty::Force));
        // In a file, the clearing may come after what it clears, as in OpenSSH.
        let options = read(
            "LocalForward 8080 127.0.0.1:80\nClearAllForwardings yes\n",
            "h",
        )
        .unwrap();
        options.validate().unwrap();
        for text in [
            "LocalForward 8080 127.0.0.1:80\n",
            "RemoteForward 9 h:9\n",
            "DynamicForward 1080\n",
        ] {
            let error = read(text, "h")
                .unwrap()
                .validate()
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("forwards nothing"), "{text}: {error}");
        }
        assert!(read("ForwardAgent yes\n", "h").is_err());
        assert!(read("RequestTTY sometimes\n", "h").is_err());
    }

    #[test]
    fn ignore_unknown_rescues_a_named_keyword_and_no_pattern() {
        assert!(read("IgnoreUnknown UseKeychain\nUseKeychain yes\n", "h").is_ok());
        assert!(read("IgnoreUnknown a,UseKeychain\nUseKeychain yes\n", "h").is_ok());
        // Only what came before it, as in OpenSSH.
        assert!(read("UseKeychain yes\nIgnoreUnknown UseKeychain\n", "h").is_err());
        // Not one whose absence changes whom td-ssh trusts or what runs.
        for name in NOT_WAIVED {
            let error = read(&format!("IgnoreUnknown {name}\n{name} x\n"), "h")
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("does not waive"), "{name}: {error}");
        }
        assert!(read("IgnoreUnknown a, UseKeychain\nUseKeychain yes\n", "h").is_err());
        for pattern in ["*", "use*", "?seKeychain", "!x"] {
            let error = read(&format!("IgnoreUnknown {pattern}\n"), "h")
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("not patterns"), "{pattern}: {error}");
        }
    }

    #[test]
    fn match_and_include_refuse_wherever_they_stand() {
        for text in [
            "Match all\n",
            "Match host h\n  Port 1\n",
            "Include other\n",
            "IgnoreUnknown match,include\nMatch all\n",
            "IgnoreUnknown match,include\nInclude other\n",
            "Host other\n  Include other\n",
            "Host other\n  Match all\n",
        ] {
            let error = read(text, "h").err().unwrap().to_string();
            assert!(error.contains("not supported"), "{text}: {error}");
        }
        for option in ["Match=all", "Include=other", "Host=h"] {
            assert!(Options::default().apply_option(option).is_err(), "{option}");
        }
    }

    #[test]
    fn no_port_is_zero() {
        assert_eq!(parse_port("22"), Some(22));
        assert_eq!(parse_port("65535"), Some(65535));
        for bad in ["0", "00", "65536", "", "x", "-1"] {
            assert_eq!(parse_port(bad), None, "{bad}");
        }
        assert!(read("Port 0\n", "h").is_err());
        assert!(Options::default().apply_option("Port=0").is_err());
    }

    #[test]
    fn algorithm_lists_filter_and_empty_refuses() {
        assert_eq!(
            filter_algorithms("+ssh-rsa", &[HOST_KEY_ALGORITHM]),
            [HOST_KEY_ALGORITHM]
        );
        assert_eq!(
            filter_algorithms("-curve*", DEFAULT_KEX),
            ["mlkem768x25519-sha256"]
        );
        assert_eq!(
            filter_algorithms("curve25519-sha256", DEFAULT_KEX),
            ["curve25519-sha256"]
        );
        for text in [
            "KexAlgorithms diffie-hellman-group14-sha1",
            "Ciphers aes128-ctr",
            "HostKeyAlgorithms ssh-rsa",
        ] {
            assert!(read(text, "h").unwrap().validate().is_err(), "{text}");
        }
        // Only the first occurrence decides.
        let options = read(
            "Ciphers chacha20-poly1305@openssh.com\nCiphers aes128-ctr\n",
            "h",
        );
        options.unwrap().validate().unwrap();
    }

    #[test]
    fn paths_expand_tokens_and_refuse_unknown_ones() {
        let expand = |value: &str| {
            expand_path(
                value,
                Path::new("/home/l"),
                "real",
                "alias",
                "ruser",
                "luser",
                22,
            )
        };
        assert_eq!(
            expand("~/.ssh/%h-%r-%n-%u-%p-%%").unwrap(),
            Path::new("/home/l/.ssh/real-ruser-alias-luser-22-%")
        );
        assert_eq!(expand("%d/x").unwrap(), Path::new("/home/l/x"));
        assert!(expand("%Z").is_err());
        assert!(expand("%").is_err());
    }

    #[test]
    fn invalid_values_name_the_keyword_and_line() {
        let error = read("\nPort nope\n", "h").err().unwrap().to_string();
        assert!(error.contains("line 2"), "{error}");
        assert!(error.contains("Port"), "{error}");
    }
}
