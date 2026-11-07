//! The client's command-line options and `ssh_config` subset. Every keyword is
//! in one of a few explicit classes (DESIGN.md section 6), because a config file
//! that asks for a removed feature must not silently connect some other way.

use crate::error::{Error, Result};
use crate::kex::{CIPHER, DEFAULT_KEX, HOST_KEY_ALGORITHM};
use crate::pattern::{glob, match_list, ListMatch};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const MAX_INCLUDE_DEPTH: usize = 8;
const MAX_CONFIG_FILE: u64 = 1024 * 1024;
const SYSTEM_CONFIG_DIR: &str = "/etc/ssh";
/// Rekeying each packet would hold up every byte behind a host-key signature.
const MIN_REKEY_LIMIT: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strict {
    Yes,
    AcceptNew,
    No,
    Ask,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Any,
    Inet,
    Inet6,
}

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
    pub connection_attempts: Option<u32>,
    pub server_alive_interval: Option<u64>,
    pub server_alive_count_max: Option<u32>,
    pub rekey_limit: Option<u64>,
    pub send_env: Vec<String>,
    pub family: Option<Family>,
    pub log_level: Option<LogLevel>,
    pub request_tty: Option<RequestTty>,
    pub password_prompts: Option<u32>,
    /// `Some(None)` is `EscapeChar none`.
    pub escape_char: Option<Option<u8>>,
    pub kex: Option<Vec<&'static str>>,
    pub session_none: Option<bool>,
    pub stdin_null: Option<bool>,
    forward_x11: Option<bool>,
    proxy_command: Option<String>,
    proxy_jump: Option<String>,
    remote_command: Option<String>,
    permit_local_command: Option<bool>,
    canonicalize: Option<bool>,
    bind: Option<String>,
    forwards: Vec<String>,
    tunnel: Option<bool>,
    clear_forwardings: Option<bool>,
    pubkey_auth: Option<bool>,
    preferred_ok: Option<bool>,
    ignore_unknown: Vec<String>,
    /// Algorithm keywords that came up empty, reported once validated: the
    /// first obtained value decides, as it does for everything else.
    empty_algorithms: Vec<&'static str>,
    host_key_set: bool,
    cipher_set: bool,
    pubkey_set: bool,
    /// Notes for `-v`: settings accepted and deliberately ignored.
    pub notes: Vec<String>,
}

/// What matching in a file needs to know.
pub struct MatchContext<'a> {
    pub original_host: &'a str,
    pub local_user: &'a str,
    pub home: &'a Path,
}

/// Where a configuration file came from, which decides who it must be owned by
/// and what a relative `Include` in it means. Includes inherit their parent's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// `~/.ssh/config`: checked as OpenSSH checks it, includes under `~/.ssh`.
    User,
    /// `-F FILE`: the caller's own choice, unchecked, includes under `~/.ssh`.
    Explicit,
    /// `/etc/ssh/ssh_config`: includes under `/etc/ssh`, never the user's tree.
    System,
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

fn parse_escape_char(word: &str) -> Result<Option<u8>> {
    if word.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    match word.as_bytes() {
        [single] => Ok(Some(*single)),
        [b'^', control @ 64..=127] => Ok(Some(control & 31)),
        _ => Err(Error::refused(format!("EscapeChar: bad value {word}"))),
    }
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
            // Honoured.
            "hostname" => set(&mut self.hostname, || Ok(word.clone())),
            "user" => set(&mut self.user, || Ok(word.clone())),
            "port" => set(&mut self.port, || parse_number("Port", &word)),
            "identityfile" => {
                self.identity_files.push(word);
                Ok(())
            }
            "identitiesonly" => {
                self.note("IdentitiesOnly: td-ssh has no agent, so it always applies");
                Ok(())
            }
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
            "connectionattempts" => set(&mut self.connection_attempts, || {
                parse_number("ConnectionAttempts", &word)
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
            "addressfamily" => set(&mut self.family, || {
                match word.to_ascii_lowercase().as_str() {
                    "any" => Ok(Family::Any),
                    "inet" => Ok(Family::Inet),
                    "inet6" => Ok(Family::Inet6),
                    _ => Err(Error::refused(format!("AddressFamily: bad value {word}"))),
                }
            }),
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
            "numberofpasswordprompts" => set(&mut self.password_prompts, || {
                parse_number("NumberOfPasswordPrompts", &word)
            }),
            "pubkeyauthentication" => set(&mut self.pubkey_auth, || {
                parse_bool("PubkeyAuthentication", &word)
            }),
            "escapechar" => set(&mut self.escape_char, || parse_escape_char(&word)),
            "sessiontype" => set(&mut self.session_none, || {
                match word.to_ascii_lowercase().as_str() {
                    "none" => Ok(true),
                    "default" => Ok(false),
                    "subsystem" => Err(Error::refused(
                        "SessionType subsystem: td-ssh does not support subsystems",
                    )),
                    _ => Err(Error::refused(format!("SessionType: bad value {word}"))),
                }
            }),
            "stdinnull" => set(&mut self.stdin_null, || parse_bool("StdinNull", &word)),
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
            "pubkeyacceptedalgorithms" | "pubkeyacceptedkeytypes" => {
                self.one_algorithm(&word, HOST_KEY_ALGORITHM, "PubkeyAcceptedAlgorithms", |o| {
                    &mut o.pubkey_set
                });
                Ok(())
            }
            "ciphers" => {
                self.one_algorithm(&word, CIPHER, "Ciphers", |o| &mut o.cipher_set);
                Ok(())
            }
            // The AEAD uses no MAC, so no list can break it.
            "macs" => Ok(()),
            // Refused unless disabled, judged on the effective value below.
            "proxycommand" => set(&mut self.proxy_command, || Ok(value.to_string())),
            "proxyjump" => set(&mut self.proxy_jump, || Ok(word.clone())),
            "remotecommand" => set(&mut self.remote_command, || Ok(value.to_string())),
            "permitlocalcommand" => set(&mut self.permit_local_command, || {
                parse_bool("PermitLocalCommand", &word)
            }),
            "forwardx11" => set(&mut self.forward_x11, || parse_bool("ForwardX11", &word)),
            "canonicalizehostname" => set(&mut self.canonicalize, || {
                Ok(!word.eq_ignore_ascii_case("no") && !word.eq_ignore_ascii_case("false"))
            }),
            "bindaddress" | "bindinterface" => set(&mut self.bind, || Ok(word.clone())),
            "localforward" | "remoteforward" | "dynamicforward" => {
                self.forwards.push(keyword.to_string());
                Ok(())
            }
            "tunnel" => set(&mut self.tunnel, || {
                Ok(!word.eq_ignore_ascii_case("no") && !word.eq_ignore_ascii_case("false"))
            }),
            "clearallforwardings" => set(&mut self.clear_forwardings, || {
                parse_bool("ClearAllForwardings", &word)
            }),
            // The only version there is. Anything else asks for what is not here.
            "protocol" if word == "2" => {
                self.note("protocol: ignored");
                Ok(())
            }
            "protocol" => Err(Error::refused(format!(
                "Protocol {word} is not supported by td-ssh"
            ))),
            "setenv"
            | "knownhostscommand"
            | "pkcs11provider"
            | "forkafterauthentication"
            | "stdiniforward"
            | "certificatefile" => Err(Error::refused(format!(
                "{keyword} is not supported by td-ssh"
            ))),
            // Convenience, inert here by construction. `ForwardX11Trusted`,
            // `LocalCommand` and the `Canonical*` settings do nothing without a parent
            // that `validate` refuses.
            "canonicaldomains"
            | "canonicalizefallbacklocal"
            | "canonicalizemaxdots"
            | "canonicalizepermittedcnames"
            | "forwardagent"
            | "forwardx11trusted"
            | "forwardx11timeout"
            | "addkeystoagent"
            | "identityagent"
            | "controlmaster"
            | "controlpath"
            | "controlpersist"
            | "compression"
            | "updatehostkeys"
            | "verifyhostkeydns"
            | "checkhostip"
            | "passwordauthentication"
            | "kbdinteractiveauthentication"
            | "challengeresponseauthentication"
            | "gssapiauthentication"
            | "gssapidelegatecredentials"
            | "hostbasedauthentication"
            | "tcpkeepalive"
            | "visualhostkey"
            | "ipqos"
            | "exitonforwardfailure"
            | "gatewayports"
            | "localcommand"
            | "nohostauthenticationforlocalhost"
            | "securitykeyprovider"
            | "fingerprinthash"
            | "enableescapecommandline"
            | "logverbose"
            | "kbdinteractivedevices"
            | "requiredrsasize"
            | "obscurekeystroketiming"
            | "warnweakcrypto"
            | "syslogfacility" => {
                self.note(&format!("{keyword}: ignored"));
                Ok(())
            }
            "ignoreunknown" => {
                self.ignore_unknown.extend(
                    word.split(',')
                        .map(str::to_ascii_lowercase)
                        .collect::<Vec<_>>(),
                );
                Ok(())
            }
            other => {
                if self
                    .ignore_unknown
                    .iter()
                    .any(|pattern| glob(pattern, other))
                {
                    Ok(())
                } else {
                    Err(Error::refused(format!("Bad configuration option: {other}")))
                }
            }
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

    fn note(&mut self, text: &str) {
        self.notes.push(text.to_string());
    }

    /// Judges the effective values once every source has been read.
    pub fn validate(&self) -> Result<()> {
        if let Some(name) = self.empty_algorithms.first() {
            return Err(Error::refused(format!(
                "{name}: no algorithm td-ssh supports remains"
            )));
        }
        let enabled = |value: &Option<String>| {
            value
                .as_deref()
                .is_some_and(|text| !text.eq_ignore_ascii_case("none"))
        };
        for (name, value) in [
            ("ProxyCommand", &self.proxy_command),
            ("ProxyJump", &self.proxy_jump),
            ("RemoteCommand", &self.remote_command),
        ] {
            if enabled(value) {
                return Err(Error::refused(format!("{name} is not supported by td-ssh")));
            }
        }
        if self.permit_local_command == Some(true) {
            return Err(Error::refused(
                "PermitLocalCommand is not supported by td-ssh",
            ));
        }
        if self.forward_x11 == Some(true) {
            return Err(Error::refused("ForwardX11 is not supported by td-ssh"));
        }
        if self.canonicalize == Some(true) {
            return Err(Error::refused(
                "CanonicalizeHostname is not supported by td-ssh",
            ));
        }
        if self.bind.is_some() {
            return Err(Error::refused(
                "BindAddress and BindInterface are not supported by td-ssh",
            ));
        }
        if self.tunnel == Some(true) {
            return Err(Error::refused("Tunnel is not supported by td-ssh"));
        }
        if self.clear_forwardings != Some(true) {
            if let Some(forward) = self.forwards.first() {
                return Err(Error::refused(format!(
                    "{forward}: forwarding is not supported by td-ssh"
                )));
            }
        }
        if self.pubkey_auth == Some(false) || self.preferred_ok == Some(false) {
            return Err(Error::refused("td-ssh authenticates only with public keys"));
        }
        Ok(())
    }

    /// Reads one configuration file, following `Host`, `Match` and `Include`.
    pub fn read_file(
        &mut self,
        path: &Path,
        context: &MatchContext<'_>,
        origin: Origin,
    ) -> Result<()> {
        self.read_file_at(path, context, origin, 0)
    }

    fn read_file_at(
        &mut self,
        path: &Path,
        context: &MatchContext<'_>,
        origin: Origin,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_INCLUDE_DEPTH {
            return Err(Error::refused(format!(
                "{}: too many nested Include directives",
                path.display()
            )));
        }
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
        self.read_text(&text, context, origin, depth, &path.display().to_string())
    }

    /// Exposed so tests read text without touching the filesystem.
    pub fn read_text(
        &mut self,
        text: &str,
        context: &MatchContext<'_>,
        origin: Origin,
        depth: usize,
        source: &str,
    ) -> Result<()> {
        let mut active = true;
        for (index, line) in text.lines().enumerate() {
            let Some((keyword, value)) = split_line(line) else {
                continue;
            };
            let located =
                |error: Error| Error::refused(format!("{source} line {}: {error}", index + 1));
            match keyword.as_str() {
                "host" => active = host_matches(&value, context.original_host),
                "match" => active = self.match_block(&value, context).map_err(located)?,
                "include" if active => {
                    for name in words(&value) {
                        for included in expand_include(&name, context.home, origin)? {
                            self.read_file_at(&included, context, origin, depth + 1)
                                .map_err(located)?;
                        }
                    }
                }
                _ if active => self.apply_keyword(&keyword, &value).map_err(located)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn match_block(&self, value: &str, context: &MatchContext<'_>) -> Result<bool> {
        let tokens = words(value);
        let mut iter = tokens.iter();
        let mut result = true;
        while let Some(token) = iter.next() {
            let lower = token.to_ascii_lowercase();
            let (negated, criterion) = match lower.strip_prefix('!') {
                Some(rest) => (true, rest.to_string()),
                None => (false, lower),
            };
            let matched = match criterion.as_str() {
                "all" => true,
                "host" | "originalhost" | "user" | "localuser" => {
                    let pattern = iter.next().ok_or_else(|| {
                        Error::refused(format!("Match {criterion}: missing argument"))
                    })?;
                    let subject: &str = match criterion.as_str() {
                        "host" => self.hostname.as_deref().unwrap_or(context.original_host),
                        "originalhost" => context.original_host,
                        "user" => self.user.as_deref().unwrap_or(context.local_user),
                        _ => context.local_user,
                    };
                    match_list(pattern, subject) == ListMatch::Accepted
                }
                other => {
                    return Err(Error::refused(format!(
                        "Match {other} is not supported by td-ssh"
                    )))
                }
            };
            if matched == negated {
                result = false;
            }
        }
        Ok(result)
    }

    pub fn effective_host_alias<'a>(&'a self, host: &'a str) -> &'a str {
        self.host_key_alias.as_deref().unwrap_or(host)
    }
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

/// A relative `Include` is under `~/.ssh` for the user's files and `/etc/ssh`
/// for the system's, which a user-writable directory must not extend.
fn include_path(pattern: &str, home: &Path, origin: Origin) -> PathBuf {
    if let Some(rest) = pattern.strip_prefix("~/") {
        home.join(rest)
    } else if pattern.starts_with('/') {
        PathBuf::from(pattern)
    } else if origin == Origin::System {
        Path::new(SYSTEM_CONFIG_DIR).join(pattern)
    } else {
        home.join(".ssh").join(pattern)
    }
}

/// `~` and a single-component-level `*`/`?`, sorted, as the file's readers
/// expect a reproducible order.
fn expand_include(pattern: &str, home: &Path, origin: Origin) -> Result<Vec<PathBuf>> {
    let path = include_path(pattern, home, origin);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if !name.contains(['*', '?']) {
        return Ok(if path.exists() {
            vec![path]
        } else {
            Vec::new()
        });
    }
    let parent = path.parent().unwrap_or(Path::new("/"));
    if parent.to_str().is_some_and(|p| p.contains(['*', '?'])) {
        return Err(Error::refused(
            "Include: a wildcard is only supported in the last path component",
        ));
    }
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Ok(Vec::new());
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|text| glob(name, text))
        })
        .map(|entry| entry.path())
        .collect();
    found.sort();
    Ok(found)
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

    fn context<'a>(host: &'a str) -> MatchContext<'a> {
        MatchContext {
            original_host: host,
            local_user: "local",
            home: Path::new("/home/local"),
        }
    }

    fn read(text: &str, host: &str) -> Result<Options> {
        let mut options = Options::default();
        options.read_text(text, &context(host), Origin::Explicit, 0, "test")?;
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

    #[test]
    fn a_relative_include_stays_under_its_origin() {
        let home = Path::new("/home/l");
        assert_eq!(
            include_path("conf.d/x", home, Origin::System),
            Path::new("/etc/ssh/conf.d/x")
        );
        for origin in [Origin::User, Origin::Explicit] {
            assert_eq!(
                include_path("conf.d/x", home, origin),
                Path::new("/home/l/.ssh/conf.d/x")
            );
        }
        for origin in [Origin::User, Origin::Explicit, Origin::System] {
            assert_eq!(include_path("/abs/x", home, origin), Path::new("/abs/x"));
            assert_eq!(include_path("~/x", home, origin), Path::new("/home/l/x"));
        }
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
        options
            .read_file(&path, &context("h"), Origin::User)
            .unwrap();
        assert_eq!(options.port, Some(1));
        write_mode(&path, "Port 2\n", 0o664);
        let error = Options::default()
            .read_file(&path, &context("h"), Origin::User)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            error,
            format!("Bad owner or permissions on {}", path.display())
        );
        // `-F` and the system file are not held to it.
        for origin in [Origin::Explicit, Origin::System] {
            let mut options = Options::default();
            options.read_file(&path, &context("h"), origin).unwrap();
            assert_eq!(options.port, Some(2));
        }
    }

    #[test]
    fn what_a_user_config_includes_is_held_to_the_same_rule() {
        let dir = td_fs::TempDir::new("td-ssh-config").unwrap();
        let included = dir.path().join("included");
        write_mode(&included, "Port 3\n", 0o666);
        let path = dir.path().join("config");
        write_mode(&path, &format!("Include {}\n", included.display()), 0o600);
        let error = Options::default()
            .read_file(&path, &context("h"), Origin::User)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("Bad owner or permissions"), "{error}");
        let mut options = Options::default();
        options
            .read_file(&path, &context("h"), Origin::Explicit)
            .unwrap();
        assert_eq!(options.port, Some(3));
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
    fn session_type_and_stdin_null_are_read_and_subsystem_is_refused() {
        assert_eq!(
            read("SessionType none\n", "h").unwrap().session_none,
            Some(true)
        );
        assert_eq!(
            read("SessionType default\n", "h").unwrap().session_none,
            Some(false)
        );
        let error = read("SessionType subsystem\n", "h")
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("subsystems"), "{error}");
        assert!(read("SessionType nonsense\n", "h").is_err());
        assert_eq!(read("StdinNull yes\n", "h").unwrap().stdin_null, Some(true));
        assert!(read("StdinNull maybe\n", "h").is_err());
    }

    #[test]
    fn escape_chars_take_openssh_s_three_forms() {
        let escape = |text: &str| read(&format!("EscapeChar {text}\n"), "h").map(|o| o.escape_char);
        assert_eq!(escape("none").unwrap(), Some(None));
        assert_eq!(escape("NONE").unwrap(), Some(None));
        assert_eq!(escape("%").unwrap(), Some(Some(b'%')));
        assert_eq!(escape("~").unwrap(), Some(Some(b'~')));
        assert_eq!(escape("^A").unwrap(), Some(Some(1)));
        assert_eq!(escape("^]").unwrap(), Some(Some(0x1d)));
        // A lone caret is a character like any other.
        assert_eq!(escape("^").unwrap(), Some(Some(b'^')));
        for bad in ["ab", "^AB", "^1"] {
            assert!(escape(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn options_whose_parent_is_refused_are_ignored_and_the_parents_refuse() {
        for text in [
            "LocalCommand /bin/echo hi\n",
            "ForwardX11Trusted yes\n",
            "ForwardX11 no\n",
        ] {
            read(text, "h").unwrap().validate().unwrap();
        }
        for text in ["PermitLocalCommand yes\n", "ForwardX11 yes\n"] {
            let error = read(text, "h").unwrap().validate().err().unwrap();
            assert!(error.to_string().contains("not supported"), "{error}");
        }
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
    fn match_criteria_and_refusals() {
        let text = "Match host h user local\n  Port 1\nMatch all\n  Port 2\n";
        assert_eq!(read(text, "h").unwrap().port, Some(1));
        assert_eq!(read(text, "other").unwrap().port, Some(2));
        let text = "Match !host h\n  Port 3\n";
        assert_eq!(read(text, "h").unwrap().port, None);
        let error = read("Match exec \"true\"\n Port 1\n", "h").err().unwrap();
        assert!(error.to_string().contains("exec"), "{error}");
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
            "Compression=no",
            "LogLevel=INFO",
            "HashKnownHosts=yes",
            "VerifyHostKeyDNS=no",
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

    #[test]
    fn connection_changing_settings_refuse_unless_disabled_first() {
        for text in [
            "ProxyCommand nc %h %p",
            "ProxyJump bastion",
            "RemoteCommand tmux",
            "PermitLocalCommand yes",
            "CanonicalizeHostname always",
            "BindAddress 10.0.0.1",
            "LocalForward 1 h:1",
            "RemoteForward 1 h:1",
            "DynamicForward 1",
            "Tunnel yes",
            "PubkeyAuthentication no",
            "PreferredAuthentications password",
        ] {
            let options = read(text, "h").unwrap();
            assert!(options.validate().is_err(), "{text}");
        }
        // td-remote's overrides beat the same keyword in a config file.
        let mut options = Options::default();
        options.apply_option("RemoteCommand=none").unwrap();
        options.apply_option("ClearAllForwardings=yes").unwrap();
        options
            .read_text(
                "RemoteCommand tmux\nLocalForward 1 h:1\n",
                &context("h"),
                Origin::Explicit,
                0,
                "test",
            )
            .unwrap();
        options.validate().unwrap();
    }

    #[test]
    fn inert_conveniences_are_noted_not_refused() {
        let options = read(
            "ForwardAgent yes\nControlMaster auto\nControlPersist 10m\nCompression yes\nAddKeysToAgent yes\n",
            "h",
        )
        .unwrap();
        options.validate().unwrap();
        assert_eq!(options.notes.len(), 5);
    }

    #[test]
    fn protocol_2_is_ignored_and_any_other_is_refused() {
        let options = read("Protocol 2\n", "h").unwrap();
        options.validate().unwrap();
        assert_eq!(options.notes, ["protocol: ignored"]);
        for other in ["1", "2,1", "1,2", "3"] {
            let text = read(&format!("Protocol {other}\n"), "h")
                .err()
                .unwrap()
                .to_string();
            assert!(text.contains("Protocol"), "{text}");
        }
    }

    /// Common beside `CanonicalizeHostname no`; with it on, that keyword refuses.
    #[test]
    fn canonicalization_settings_mean_nothing_while_it_is_off() {
        let settings = "CanonicalDomains a.example\nCanonicalizeFallbackLocal yes\nCanonicalizeMaxDots 1\nCanonicalizePermittedCNAMEs *.a:*.b\n";
        let off = read(&format!("CanonicalizeHostname no\n{settings}"), "h").unwrap();
        off.validate().unwrap();
        assert_eq!(off.notes.len(), 4);
        read(settings, "h").unwrap().validate().unwrap();
        let on = read(&format!("CanonicalizeHostname yes\n{settings}"), "h").unwrap();
        assert!(on.validate().is_err());
    }

    #[test]
    fn set_env_and_unknown_keywords_refuse_and_ignore_unknown_rescues() {
        assert!(read("SetEnv A=b\n", "h").is_err());
        assert!(read("NotAKeyword x\n", "h").is_err());
        assert!(read("IgnoreUnknown UseKeychain\nUseKeychain yes\n", "h").is_ok());
        assert!(read("IgnoreUnknown use*\nUseKeychain yes\n", "h").is_ok());
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
