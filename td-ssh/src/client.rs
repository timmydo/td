//! The `ssh` client: connect, verify the host, authenticate, and run one
//! session channel.

use crate::client_config::{
    caller_uid, expand_path, parse_port, LogLevel, Options, Origin, RequestTty, Strict,
};
use crate::conn::{
    self, Conn, Credit, Event, EventReceiver, LocalWindow, WhenBroken, CHANNEL_MAX_PACKET,
    CHANNEL_WINDOW,
};
use crate::error::{
    diagnostic, peer_text, primary_output, set_terminal_raw, Error, Patient, Result,
};
use crate::kex::{
    self, ed25519_signature_blob, parse_ed25519_public, Kex, KexEnvironment, RandomSource, Role,
    CIPHER, DEFAULT_KEX, HOST_KEY_ALGORITHM,
};
use crate::keyfile::{fingerprint_of_blob, PrivateKey, PrivateKeyFile};
use crate::known_hosts::{self, Lookup};
use crate::msg;
use crate::packet::{exchange_versions, PacketReader, PacketWriter, IDENTIFICATION, MAX_PACKET};
use crate::pattern::glob;
use crate::prompt::{host_key_question, Prompter};
use crate::sys;
use crate::wire::{Reader, Writer};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, Shutdown, TcpStream, ToSocketAddrs};
use std::os::fd::AsFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use td_encoding::{base64, from_base64};

pub const EXIT_FAILURE: i32 = 255;
const SESSION_CHANNEL: u32 = 0;
const WINDOW_POLL: Duration = Duration::from_millis(250);
const TICK: Duration = Duration::from_secs(1);
const DRAIN_POLL: Duration = Duration::from_millis(10);
/// A goodbye to a peer that stopped reading must not hold up the exit.
const FAREWELL_WRITE: Duration = Duration::from_secs(1);
/// Every session has a bound on one write, keepalive or no keepalive: a peer that
/// stops reading blocks the only thread that could notice anything else.
const SESSION_WRITE_LIMIT: Duration = Duration::from_secs(900);
/// What a core that frees itself takes after `~.` shut its socket, with room to spare.
const ESCAPE_LAST_RESORT: Duration = Duration::from_secs(3);
/// Chunks of typed input a shut window can hold back while `~.` is still looked for.
const ESCAPE_BACKLOG: usize = 256;
/// What fits one packet, whose bound is `MAX_PACKET`, with the framing and padding
/// of an `exec` request to spare (DESIGN.md section 6).
pub const MAX_REMOTE_COMMAND: usize = MAX_PACKET - 64;
const DEFAULT_ALIVE_COUNT: u32 = 3;
const DEFAULT_ESCAPE: u8 = b'~';
/// Wrong answers to the host-key question before the connection is refused,
/// whichever source is answering.
const HOST_KEY_TRIES: u32 = 3;
/// Wrong passphrases for one identity before the next is tried.
const PASSPHRASE_TRIES: u32 = 3;

/// Everything the command line settled.
#[derive(Default)]
pub struct Invocation {
    pub options: Options,
    pub config_file: Option<PathBuf>,
    pub destination: String,
    /// Bytes, since a remote command need not be text.
    pub command: Option<Vec<u8>>,
    pub identity_args: Vec<String>,
    pub stdin_null: bool,
    pub no_session: bool,
    pub force_tty: u8,
    pub no_tty: bool,
    pub dump_config: bool,
    pub verbosity: u8,
    pub quiet: bool,
    /// `-4` is `Some(false)` and `-6` `Some(true)`, the last one given deciding, as
    /// Git passes for `git fetch -4`.
    pub ipv6: Option<bool>,
}

/// Flags that take an argument, for parsing `-pPORT` and `-p PORT` alike.
const TAKES_VALUE: &[char] = &['F', 'i', 'l', 'o', 'p'];
/// `td-remote` passes it for a stock `ssh`'s sake: it disables X11, which td-ssh never
/// does. Its `-a` is `ForwardAgent no`, so a person's `ForwardAgent yes` reads.
const NO_OP_FLAGS: &[char] = &['x'];

pub enum Parsed {
    Run(Box<Invocation>),
    PrintAndExit(String),
}

/// An option or destination word, which must be text. Only the remote command
/// is carried as bytes.
fn text_argument(arg: &OsString) -> Result<&str> {
    arg.to_str()
        .ok_or_else(|| Error::refused(format!("argument is not UTF-8: {arg:?}")))
}

pub fn parse_args(args: &[OsString]) -> Result<Parsed> {
    let mut invocation = Invocation::default();
    let mut index = 0usize;
    let mut destination: Option<String> = None;
    let mut command: Vec<&[u8]> = Vec::new();
    while let Some(arg) = args.get(index) {
        index += 1;
        // Options may follow the destination, up to the first command word.
        let starts_option = arg.as_bytes().first() == Some(&b'-') && arg.as_bytes() != b"-";
        if !command.is_empty() || !starts_option {
            place_positional(arg, &mut destination, &mut command, &mut invocation.options)?;
            continue;
        }
        if arg.as_bytes() == b"--" {
            for rest in args.get(index..).unwrap_or_default() {
                place_positional(
                    rest,
                    &mut destination,
                    &mut command,
                    &mut invocation.options,
                )?;
            }
            break;
        }
        let arg = text_argument(arg)?;
        let flags = arg.char_indices().skip(1);
        for (at, flag) in flags {
            if TAKES_VALUE.contains(&flag) {
                let attached = arg.get(at + flag.len_utf8()..).unwrap_or_default();
                let value = if attached.is_empty() {
                    let value = args
                        .get(index)
                        .ok_or_else(|| Error::refused(format!("option -{flag} needs a value")))?;
                    index += 1;
                    text_argument(value)?
                } else {
                    attached
                };
                apply_value_flag(flag, value, &mut invocation)?;
                break;
            }
            match flag {
                '4' => invocation.ipv6 = Some(false),
                '6' => invocation.ipv6 = Some(true),
                'a' => invocation.options.apply_option("ForwardAgent=no")?,
                'G' => invocation.dump_config = true,
                'n' => invocation.stdin_null = true,
                'N' => invocation.no_session = true,
                'q' => invocation.quiet = true,
                't' => invocation.force_tty = invocation.force_tty.saturating_add(1),
                'T' => invocation.no_tty = true,
                'v' => invocation.verbosity = invocation.verbosity.saturating_add(1),
                'V' => return Ok(Parsed::PrintAndExit(format!("{IDENTIFICATION}\n"))),
                other if NO_OP_FLAGS.contains(&other) => {}
                other => return Err(Error::refused(format!("unknown option -- {other}"))),
            }
        }
    }
    invocation.destination = destination.ok_or_else(|| {
        Error::refused("usage: ssh [-46aGNnqTtVvx] [-F file] [-i identity] [-l user] [-o option] [-p port] destination [command...]")
    })?;
    if !command.is_empty() {
        invocation.command = Some(command.join(&b' '));
    }
    Ok(Parsed::Run(Box::new(invocation)))
}

fn place_positional<'a>(
    arg: &'a OsString,
    destination: &mut Option<String>,
    command: &mut Vec<&'a [u8]>,
    options: &mut Options,
) -> Result<()> {
    if destination.is_none() {
        let text = text_argument(arg)?;
        // Its user and port count here: an option after it comes too late. One that
        // does not parse is refused where it is used.
        if let Ok((user, _, port)) = parse_destination(text) {
            options.user = options.user.take().or(user);
            options.port = options.port.or(port);
        }
        *destination = Some(text.to_string());
    } else {
        command.push(arg.as_bytes());
    }
    Ok(())
}

fn apply_value_flag(flag: char, value: &str, invocation: &mut Invocation) -> Result<()> {
    match flag {
        'F' => invocation.config_file = Some(PathBuf::from(value)),
        'i' => invocation.identity_args.push(value.to_string()),
        // As `-o User` and `-o Port`: the first obtained stands, so a later one is
        // not even looked at, as OpenSSH does not.
        'l' => {
            invocation
                .options
                .user
                .get_or_insert_with(|| value.to_string());
        }
        'o' => invocation.options.apply_option(value)?,
        'p' => {
            if invocation.options.port.is_none() {
                invocation.options.port = Some(
                    parse_port(value)
                        .ok_or_else(|| Error::refused(format!("Bad port '{value}'")))?,
                );
            }
        }
        _ => return Err(Error::refused(format!("unknown option -- {flag}"))),
    }
    Ok(())
}

/// `ssh://[user@]host[:port]`, `user@host`, or `host`.
fn parse_destination(destination: &str) -> Result<(Option<String>, String, Option<u16>)> {
    if let Some(rest) = destination.strip_prefix("ssh://") {
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        let (user, host_port) = split_user(rest);
        let (host, port) = match host_port.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') || host.ends_with(']') => {
                let parsed =
                    parse_port(port).ok_or_else(|| Error::refused(format!("Bad port '{port}'")))?;
                (host, Some(parsed))
            }
            _ => (host_port, None),
        };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        return Ok((user, checked_host(host)?, port));
    }
    let (user, host) = split_user(destination);
    Ok((user, checked_host(host)?, None))
}

/// The destination as everything after the command line sees it. Lowercased
/// once, as OpenSSH does, since an HMAC over `HOST` would miss a pin for `host`.
fn lowered_destination(destination: &str) -> Result<(Option<String>, String, Option<u16>)> {
    let (user, host, port) = parse_destination(destination)?;
    Ok((user, host.to_ascii_lowercase(), port))
}

/// `HostName` as a connection and a host-key lookup use it, lowered for the
/// same reason as the destination.
fn target_host(options: &Options, original_host: &str) -> String {
    options
        .hostname
        .as_deref()
        .map_or_else(
            || original_host.to_string(),
            |name| name.replace("%h", original_host),
        )
        .to_ascii_lowercase()
}

fn split_user(text: &str) -> (Option<String>, &str) {
    match text.rsplit_once('@') {
        Some((user, host)) => (Some(user.to_string()), host),
        None => (None, text),
    }
}

/// OpenSSH's `valid_hostname`: what a shell or a `known_hosts` pattern could read
/// as something else, with `*` and `?` for the patterns. A comma would make a
/// plain pin two hosts.
const HOST_REFUSED: &str = "'`\"$\\;&<>|(){},*?";

/// A destination is never an option or a shell of its own: git passes
/// user-supplied hosts, and a leading dash has been an injection route before.
fn checked_host(host: &str) -> Result<String> {
    if host.is_empty()
        || host.starts_with('-')
        || host
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || HOST_REFUSED.contains(c))
    {
        return Err(Error::refused(format!(
            "hostname contains invalid characters: {host:?}"
        )));
    }
    Ok(host.to_string())
}

pub struct Logger {
    level: LogLevel,
}

impl Logger {
    pub fn debug(&self, text: &str) {
        if self.level >= LogLevel::Debug {
            diagnostic(&format!("debug1: {text}"));
        }
    }

    pub fn info(&self, text: &str) {
        if self.level >= LogLevel::Info {
            diagnostic(text);
        }
    }

    pub fn error(&self, text: &str) {
        if self.level >= LogLevel::Error {
            diagnostic(text);
        }
    }
}

/// Answers `KexEnvironment` for the client, pinning the first host key.
struct ClientEnv {
    random: RandomSource<File>,
    check: HostCheck,
    pinned: Option<Vec<u8>>,
}

impl KexEnvironment for ClientEnv {
    fn random(&mut self, out: &mut [u8]) -> Result<()> {
        self.random.fill(out)
    }

    fn host_key_blob(&mut self) -> Result<Vec<u8>> {
        Err(Error::protocol("the client holds no host key"))
    }

    fn sign(&mut self, _hash: &[u8; 32]) -> Result<Vec<u8>> {
        Err(Error::protocol("the client holds no host key"))
    }

    fn check_host_key(&mut self, blob: &[u8]) -> Result<()> {
        match &self.pinned {
            Some(pinned) if pinned.as_slice() == blob => Ok(()),
            Some(_) => Err(Error::protocol("host key changed during a rekey")),
            None => {
                self.check.check(blob)?;
                self.pinned = Some(blob.to_vec());
                Ok(())
            }
        }
    }

    fn host_key_verified(&mut self) {
        self.check.record_accepted();
    }
}

/// Across every file. `Corrupt` says where, for a message that can be acted on.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Match,
    Revoked,
    Mismatch,
    Corrupt(String),
    Unknown,
}

struct HostCheck {
    name: String,
    display_host: String,
    address: String,
    user_files: Vec<PathBuf>,
    global_files: Vec<PathBuf>,
    strict: Strict,
    hash: bool,
    batch: bool,
    logger: Logger,
    /// A key accepted as new, waiting for its signature over this exchange.
    accepted: Option<String>,
    /// Time at the question does not count against the setup's deadline.
    clock: SetupClock,
}

/// A file that is not there holds no pins. One that cannot be read may hold
/// any, so it must not count as empty.
fn read_known_hosts(path: &Path) -> Result<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(Error::refused(format!(
            "Cannot read known hosts file {}: {error}",
            path.display()
        ))),
    }
}

fn other_key_warning(path: &Path, name: &str, other: &known_hosts::OtherKey) -> String {
    let fingerprint = from_base64(&other.key_b64)
        .map_or_else(|| "unknown".to_string(), |blob| fingerprint_of_blob(&blob));
    format!(
        "WARNING: {algorithm} key found for host {name}\nin {path}:{line}\n{algorithm} key fingerprint {fingerprint}.",
        algorithm = other.algorithm,
        path = path.display(),
        line = other.line,
    )
}

impl HostCheck {
    /// The verdict across every file, and a warning for each pin in a key type
    /// td-ssh cannot compare, which matter only if the verdict is `Unknown`. A
    /// corrupt line says where it is in the verdict's own text.
    fn read_files(&self, key_b64: &str) -> Result<(Verdict, Vec<String>)> {
        let mut result = Verdict::Unknown;
        let mut warnings = Vec::new();
        for path in self.user_files.iter().chain(&self.global_files) {
            let bytes = read_known_hosts(path)?;
            match known_hosts::lookup(&bytes, &self.name, key_b64) {
                Lookup::Revoked => return Ok((Verdict::Revoked, warnings)),
                Lookup::Match => {
                    result = Verdict::Match;
                }
                Lookup::Mismatch if result != Verdict::Match => {
                    match known_hosts::corrupt_pin(&bytes, &self.name, key_b64) {
                        Some(line) if !matches!(result, Verdict::Mismatch) => {
                            result = Verdict::Corrupt(format!("{}:{line}", path.display()));
                        }
                        Some(_) => {}
                        None => result = Verdict::Mismatch,
                    }
                }
                _ => {}
            }
            for other in known_hosts::other_keys(&bytes, &self.name) {
                warnings.push(other_key_warning(path, &self.name, &other));
            }
        }
        Ok((result, warnings))
    }

    fn check(&mut self, blob: &[u8]) -> Result<()> {
        // Refuses anything but a plain Ed25519 key before any lookup.
        parse_ed25519_public(blob)?;
        let key_b64 = base64(blob);
        let fingerprint = fingerprint_of_blob(blob);
        self.logger
            .debug(&format!("Server host key: ssh-ed25519 {fingerprint}"));
        let (verdict, warnings) = self.read_files(&key_b64)?;
        match verdict {
            Verdict::Match => Ok(()),
            Verdict::Revoked => {
                self.logger
                    .error(&format!("Host key for {} was revoked.", self.name));
                Err(verification_failed())
            }
            Verdict::Corrupt(place) => {
                self.logger.error(&format!(
                    "The ED25519 line for {} at {place} is corrupt: it holds no key.\n\
                     Fix or remove that line of known_hosts to go on.",
                    self.name
                ));
                Err(verification_failed())
            }
            Verdict::Mismatch => {
                self.logger.error(&format!(
                    "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
                     @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n\
                     @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
                     The ED25519 host key for {} is now {fingerprint},\n\
                     which differs from the one recorded in known_hosts.",
                    self.name
                ));
                Err(verification_failed())
            }
            Verdict::Unknown => {
                for warning in &warnings {
                    self.logger.info(warning);
                }
                if !warnings.is_empty() {
                    self.logger
                        .info("Keys of a different type are already known for this host.");
                }
                self.unknown(&key_b64, &fingerprint)
            }
        }
    }

    fn unknown(&mut self, key_b64: &str, fingerprint: &str) -> Result<()> {
        match self.strict {
            Strict::Yes => {
                self.logger.error(&format!(
                    "No ED25519 host key is known for {} and you have requested strict checking.",
                    self.name
                ));
                Err(verification_failed())
            }
            Strict::AcceptNew | Strict::No => {
                self.accepted = Some(key_b64.to_string());
                Ok(())
            }
            Strict::Ask => {
                let question = host_key_question(&self.display_host, &self.address, fingerprint);
                let prompter = Prompter::new(self.batch);
                let asked = Instant::now();
                let answer =
                    confirm_host_key(|| prompter.question(&question), fingerprint, &self.logger);
                self.clock.extend(asked.elapsed());
                answer?;
                self.accepted = Some(key_b64.to_string());
                Ok(())
            }
        }
    }

    /// The signature held, so what was accepted may be remembered.
    fn record_accepted(&mut self) {
        if let Some(key_b64) = self.accepted.take() {
            self.record(&key_b64);
        }
    }

    fn record(&self, key_b64: &str) {
        let Some(path) = self.user_files.first() else {
            return;
        };
        let random = |salt: &mut [u8; 20]| -> io::Result<()> {
            File::open("/dev/urandom").and_then(|mut file| file.read_exact(salt))
        };
        let line = match line_to_record(&self.name, key_b64, self.hash, random) {
            Ok(line) => line,
            Err(error) => {
                self.logger.error(&format!(
                    "Failed to add the host to the list of known hosts: hashing it: {error}"
                ));
                return;
            }
        };
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                let _ = fs::DirBuilder::new()
                    .recursive(true)
                    .mode_private()
                    .create(parent);
            }
        }
        match append_known_host(path, &line) {
            Ok(()) => self.logger.info(&format!(
                "Warning: Permanently added '{}' (ED25519) to the list of known hosts.",
                self.name
            )),
            Err(error) => self.logger.error(&format!(
                "Failed to add the host to the list of known hosts ({}): {error}",
                path.display()
            )),
        }
    }
}

/// What `HashKnownHosts` asked for or nothing: a line in the clear would hand a
/// reader of the file the very name hashing was to keep from it.
fn line_to_record(
    name: &str,
    key_b64: &str,
    hash: bool,
    random: impl FnOnce(&mut [u8; 20]) -> io::Result<()>,
) -> io::Result<String> {
    if !hash {
        return Ok(known_hosts::new_line(name, key_b64, None));
    }
    let mut salt = [0u8; 20];
    random(&mut salt)?;
    Ok(known_hosts::new_line(name, key_b64, Some(&salt)))
}

/// Appends a whole line, ending the file's last one first if it lacks a newline:
/// otherwise the new entry merges into it and neither is a pin.
fn append_known_host(path: &Path, line: &str) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .create(true)
        .append(true)
        .mode(0o644)
        .open(path)?;
    let mut text = String::with_capacity(line.len() + 1);
    if file.metadata()?.len() > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0u8];
        file.read_exact(&mut last)?;
        if last != *b"\n" {
            text.push('\n');
        }
    }
    text.push_str(line);
    file.write_all(text.as_bytes())
}

/// Asks at most `HOST_KEY_TRIES` times: an askpass answering nonsense must not
/// spawn for ever, and the terminal is held to the same bound.
fn confirm_host_key(
    mut ask: impl FnMut() -> Result<String>,
    fingerprint: &str,
    logger: &Logger,
) -> Result<()> {
    for _ in 0..HOST_KEY_TRIES {
        let answer = ask().map_err(|error| {
            logger.error(&error.to_string());
            verification_failed()
        })?;
        match answer.as_str() {
            "no" | "No" | "NO" => return Err(verification_failed()),
            "yes" | "Yes" | "YES" => return Ok(()),
            other if other == fingerprint => return Ok(()),
            _ => logger.error("Please type 'yes', 'no' or the fingerprint."),
        }
    }
    Err(verification_failed())
}

trait PrivateDir {
    fn mode_private(&mut self) -> &mut Self;
}

impl PrivateDir for fs::DirBuilder {
    fn mode_private(&mut self) -> &mut Self {
        use std::os::unix::fs::DirBuilderExt;
        self.mode(0o700)
    }
}

fn verification_failed() -> Error {
    Error::refused("Host key verification failed.")
}

fn connect(
    host: &str,
    port: u16,
    ipv6: Option<bool>,
    timeout: Option<Duration>,
) -> Result<TcpStream> {
    let mut last = None;
    let addresses = (host, port).to_socket_addrs().map_err(|error| {
        Error::refused(format!("ssh: Could not resolve hostname {host}: {error}"))
    })?;
    for address in addresses.filter(|address| ipv6.is_none_or(|six| address.is_ipv6() == six)) {
        let attempt = match timeout {
            Some(limit) => TcpStream::connect_timeout(&address, limit),
            None => TcpStream::connect(address),
        };
        match attempt {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err(Error::refused(format!(
        "ssh: connect to host {host} port {port}: {}",
        last.map_or_else(
            || "no address of the family asked for".to_string(),
            |error| error.to_string()
        )
    )))
}

/// `ServerAliveInterval` times its count: the time keepalive would give a peer.
fn keepalive_limit(options: &Options) -> Option<Duration> {
    let interval = options.server_alive_interval.filter(|value| *value > 0)?;
    let count = options
        .server_alive_count_max
        .unwrap_or(DEFAULT_ALIVE_COUNT)
        .max(1);
    Some(Duration::from_secs(
        interval.saturating_mul(u64::from(count)),
    ))
}

fn connect_limit(options: &Options) -> Option<Duration> {
    options
        .connect_timeout
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
}

/// How long a read may wait before the session is established, when no event
/// loop is there to send a keepalive. A local prompt does not read the socket,
/// so time spent at one does not count. `SetupClock` tightens this where
/// `ConnectTimeout` is set, up to the end of the first exchange.
fn setup_read_limit(options: &Options) -> Option<Duration> {
    connect_limit(options).or_else(|| keepalive_limit(options))
}

/// What one read may wait during authentication, which is past `ConnectTimeout`'s
/// deadline. Not the deadline's rest: a passphrase prompt can take a person longer.
fn auth_read_limit(options: &Options) -> Option<Duration> {
    keepalive_limit(options).or_else(|| connect_limit(options))
}

/// Every session's bound on one write, and `ServerAliveInterval`'s if shorter.
fn session_write_limit(options: &Options) -> Duration {
    keepalive_limit(options).map_or(SESSION_WRITE_LIMIT, |limit| limit.min(SESSION_WRITE_LIMIT))
}

/// Applied to the one socket, which every clone of it shares. A write that
/// stalls blocks the only thread that would send a keepalive or notice a miss,
/// so the socket has to bound it.
fn apply_socket_limits(stream: &TcpStream, options: &Options) -> Result<()> {
    stream.set_read_timeout(setup_read_limit(options))?;
    stream.set_write_timeout(Some(session_write_limit(options)))?;
    Ok(())
}

/// `ConnectTimeout` over the identification and the first exchange, as one
/// absolute deadline. A limit on one read is no limit on a server sending a byte
/// at a time. Shared with the host-key question, whose time does not count.
#[derive(Clone, Default)]
struct SetupClock(Arc<Mutex<Option<Instant>>>);

impl SetupClock {
    fn until(limit: Option<Duration>) -> Self {
        Self(Arc::new(Mutex::new(
            limit.map(|limit| Instant::now() + limit),
        )))
    }

    fn deadline(&self) -> Option<Instant> {
        self.0.lock().ok().and_then(|deadline| *deadline)
    }

    fn extend(&self, by: Duration) {
        if let Ok(mut deadline) = self.0.lock() {
            *deadline = deadline.map(|at| at + by);
        }
    }

    fn stop(&self) {
        if let Ok(mut deadline) = self.0.lock() {
            *deadline = None;
        }
    }
}

/// The socket as the setup reads it: the deadline re-armed before every read, so
/// `read_exact`'s loop cannot multiply it, in the manner of `ipc::UntilDeadline`.
struct SetupStream {
    stream: TcpStream,
    clock: SetupClock,
}

impl Read for SetupStream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if let Some(deadline) = self.clock.deadline() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
            self.stream.set_read_timeout(Some(remaining))?;
        }
        self.stream.read(out)
    }
}

impl Write for SetupStream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.stream.write(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// What establishing a connection reads under.
struct SetupLimits {
    clock: SetupClock,
    auth_read: Option<Duration>,
}

fn is_timeout(error: &Error) -> bool {
    matches!(
        error,
        Error::Io(error) if matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        )
    )
}

/// td-remote matches this text at the default `LogLevel`.
fn timeout_message(destination: &str) -> String {
    format!("Timeout, server {destination} not responding.")
}

/// The blocking link before the session starts. A server may start a rekey at
/// any point here, however long a passphrase prompt kept this thread from
/// reading, so messages arrive through `next_message`, which runs one to its end.
struct AuthLink<'a, R, W, E> {
    reader: &'a mut PacketReader<R>,
    writer: &'a mut PacketWriter<W>,
    kex: &'a mut Kex,
    env: &'a mut E,
}

impl<R: Read, W: Write, E: KexEnvironment> AuthLink<'_, R, W, E> {
    /// Our KEXINIT only ever answers the peer's and `next_message` then stays
    /// until that exchange ends, so nothing is ever sent mid-exchange.
    fn send(&mut self, payload: &[u8]) -> Result<()> {
        self.writer.write_packet(payload)
    }

    /// The next message for the caller, after what the transport itself owns.
    fn next_message(&mut self) -> Result<Vec<u8>> {
        loop {
            let payload = self.reader.read_packet()?;
            let number = payload.first().copied().unwrap_or(0);
            match number {
                msg::IGNORE | msg::DEBUG | msg::EXT_INFO | msg::UNIMPLEMENTED => {}
                msg::DISCONNECT => return Err(conn::disconnected(&payload)),
                number if msg::is_kex(number) => {
                    conn::kex_step(self.reader, self.writer, self.kex, self.env, &payload)?;
                }
                number if self.kex.peer_committed() => {
                    return Err(Error::protocol(format!(
                        "message {number} after the peer's KEXINIT"
                    )));
                }
                msg::GLOBAL_REQUEST => {
                    let mut request = Reader::new(payload.get(1..).unwrap_or_default());
                    let _name = request.string()?;
                    if request.bool()? {
                        self.send(&[msg::REQUEST_FAILURE])?;
                    }
                }
                _ => return Ok(payload),
            }
        }
    }
}

fn userauth_service<R: Read, W: Write, E: KexEnvironment>(
    link: &mut AuthLink<'_, R, W, E>,
) -> Result<()> {
    let mut request = Writer::message(msg::SERVICE_REQUEST);
    request.str("ssh-userauth");
    link.send(request.as_slice())?;
    let payload = link.next_message()?;
    match payload.first().copied() {
        Some(msg::SERVICE_ACCEPT) => Ok(()),
        other => Err(Error::protocol(format!(
            "unexpected message {other:?} awaiting the auth service"
        ))),
    }
}

struct Identity {
    path: PathBuf,
    file: PrivateKeyFile,
}

/// What OpenSSH refuses, and a key someone else owns: anyone else could have
/// replaced it, or read it.
fn identity_problem(uid: u32, mode: u32, caller: u32, path: &Path) -> Option<String> {
    if uid != caller {
        return Some(format!(
            "Bad owner on '{}': it is not yours.\nThis private key will be ignored.",
            path.display()
        ));
    }
    let mode = mode & 0o777;
    if mode & 0o077 != 0 {
        return Some(format!(
            "Permissions 0{mode:o} for '{}' are too open.\nThis private key will be ignored.",
            path.display()
        ));
    }
    None
}

fn load_identity(path: &Path, logger: &Logger) -> Option<Identity> {
    // Opened first, so what is judged is what is read.
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            logger.debug(&format!("identity file {} : {error}", path.display()));
            return None;
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            logger.debug(&format!("identity file {} : {error}", path.display()));
            return None;
        }
    };
    let caller = match caller_uid() {
        Ok(caller) => caller,
        Err(error) => {
            logger.error(&format!("{}: {error}", path.display()));
            return None;
        }
    };
    if let Some(problem) = identity_problem(metadata.uid(), metadata.mode(), caller, path) {
        logger.error(&problem);
        return None;
    }
    if metadata.len() as usize > crate::keyfile::MAX_PRIVATE_KEY_FILE {
        logger.error(&format!("{} is too large to be a key", path.display()));
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    if let Err(error) = file.read_to_end(&mut bytes) {
        td_primitives::ct::zero(&mut bytes);
        logger.debug(&format!("reading {}: {error}", path.display()));
        return None;
    }
    match PrivateKeyFile::parse_owned(bytes) {
        Ok(file) => Some(Identity {
            path: path.to_path_buf(),
            file,
        }),
        Err(error) => {
            logger.debug(&format!("skipping {}: {error}", path.display()));
            None
        }
    }
}

fn decrypt_identity(identity: &Identity, attempts: u32, batch: bool) -> Result<PrivateKey> {
    if !identity.file.is_encrypted() {
        return identity.file.decrypt(None);
    }
    let prompter = Prompter::new(batch);
    let prompt = format!("Enter passphrase for key '{}': ", identity.path.display());
    let mut last = Error::refused("no passphrase attempts allowed");
    for _ in 0..attempts.max(1) {
        let mut answer = prompter.secret(&prompt)?;
        if answer.is_empty() {
            return Err(Error::refused("no passphrase given"));
        }
        let result = identity.file.decrypt(Some(&answer));
        td_primitives::ct::zero(&mut answer);
        match result {
            Ok(key) => return Ok(key),
            Err(error) => last = error,
        }
    }
    Err(last)
}

fn auth_request(user: &str, method: &str) -> Writer {
    let mut request = Writer::message(msg::USERAUTH_REQUEST);
    request.str(user).str("ssh-connection").str(method);
    request
}

/// RFC 4252 section 7's signed data, which server and client must build alike.
pub fn signed_auth_data(session_id: &[u8; 32], user: &str, blob: &[u8]) -> Vec<u8> {
    let mut data = Writer::new();
    data.string(session_id);
    data.u8(msg::USERAUTH_REQUEST);
    data.str(user).str("ssh-connection").str("publickey");
    data.bool(true);
    data.str(HOST_KEY_ALGORITHM);
    data.string(blob);
    data.into_vec()
}

/// Who to log in as, and with what.
struct Login<'a> {
    user: &'a str,
    identities: &'a [Identity],
    prompts: u32,
    batch: bool,
    logger: &'a Logger,
}

fn authenticate<R: Read, W: Write, E: KexEnvironment>(
    link: &mut AuthLink<'_, R, W, E>,
    session_id: &[u8; 32],
    login: &Login<'_>,
) -> Result<()> {
    let (user, logger) = (login.user, login.logger);
    link.send(auth_request(user, "none").as_slice())?;
    if await_auth_reply(link, logger)? == AuthReply::Success {
        return Ok(());
    }
    for identity in login.identities {
        let public = identity.file.public();
        let blob = kex::ed25519_public_blob(&public);
        logger.debug(&format!(
            "Offering public key: {} ED25519 {}",
            identity.path.display(),
            fingerprint_of_blob(&blob)
        ));
        let mut query = auth_request(user, "publickey");
        query.bool(false).str(HOST_KEY_ALGORITHM).string(&blob);
        link.send(query.as_slice())?;
        match await_auth_reply(link, logger)? {
            AuthReply::Failure => continue,
            AuthReply::Success => return Ok(()),
            AuthReply::KeyOk => {}
        }
        let key = match decrypt_identity(identity, login.prompts, login.batch) {
            Ok(key) => key,
            Err(Error::Aborted) => return Err(Error::Aborted),
            Err(error) => {
                logger.error(&format!(
                    "Load key \"{}\": {error}",
                    identity.path.display()
                ));
                continue;
            }
        };
        let signature = key.sign(&signed_auth_data(session_id, user, &blob))?;
        let mut request = auth_request(user, "publickey");
        request
            .bool(true)
            .str(HOST_KEY_ALGORITHM)
            .string(&blob)
            .string(&ed25519_signature_blob(&signature));
        link.send(request.as_slice())?;
        match await_auth_reply(link, logger)? {
            AuthReply::Success => return Ok(()),
            AuthReply::Failure | AuthReply::KeyOk => {}
        }
    }
    Err(Error::refused("Permission denied (publickey)."))
}

#[derive(Debug, PartialEq, Eq)]
enum AuthReply {
    Success,
    Failure,
    KeyOk,
}

fn await_auth_reply<R: Read, W: Write, E: KexEnvironment>(
    link: &mut AuthLink<'_, R, W, E>,
    logger: &Logger,
) -> Result<AuthReply> {
    loop {
        let payload = link.next_message()?;
        match payload.first().copied() {
            Some(msg::USERAUTH_SUCCESS) => return Ok(AuthReply::Success),
            Some(msg::USERAUTH_FAILURE) => return Ok(AuthReply::Failure),
            Some(msg::USERAUTH_PK_OK) => return Ok(AuthReply::KeyOk),
            Some(msg::USERAUTH_BANNER) => {
                let mut banner = Reader::new(payload.get(1..).unwrap_or_default());
                let text = String::from_utf8_lossy(banner.string()?).into_owned();
                // The logger is what keeps a server's escapes off the terminal.
                logger.info(text.trim_end_matches(['\r', '\n']));
            }
            other => {
                return Err(Error::protocol(format!(
                    "unexpected message {other:?} during authentication"
                )))
            }
        }
    }
}

/// Restores the terminal on every exit path that runs destructors, and the
/// explicit `exit` paths call `restore` first. It is stdin's terminal: the one
/// that was tested and the one whose keys become channel data.
struct TerminalGuard {
    saved: sys::Termios,
    active: bool,
}

impl TerminalGuard {
    fn enter() -> Option<Self> {
        let saved = sys::terminal_attributes(io::stdin()).ok()?;
        sys::set_terminal_attributes(io::stdin(), &saved.raw()).ok()?;
        set_terminal_raw(true);
        Some(Self {
            saved,
            active: true,
        })
    }

    fn restore(&mut self) {
        if self.active {
            let _ = sys::set_terminal_attributes(io::stdin(), &self.saved);
            set_terminal_raw(false);
            self.active = false;
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// The escape character then `.`, after a newline, disconnects; it twice sends
/// itself. Nothing else escapes.
struct EscapeFilter {
    escape: u8,
    at_line_start: bool,
    pending: bool,
}

impl EscapeFilter {
    fn new(escape: u8) -> Self {
        Self {
            escape,
            at_line_start: true,
            pending: false,
        }
    }

    /// Filters input in place. True when the escape asks to disconnect.
    fn filter(&mut self, input: &[u8], output: &mut Vec<u8>) -> bool {
        for byte in input {
            if self.pending {
                self.pending = false;
                if *byte == b'.' {
                    return true;
                }
                output.push(self.escape);
                if *byte == self.escape {
                    self.at_line_start = false;
                    continue;
                }
            } else if self.at_line_start && *byte == self.escape {
                self.pending = true;
                continue;
            }
            output.push(*byte);
            self.at_line_start = matches!(byte, b'\r' | b'\n');
        }
        false
    }
}

/// What `~.` needs to end a session that nothing else can.
struct EscapeExit {
    events: conn::EventSender,
    escaped: Arc<AtomicBool>,
    /// The socket, to end a write the core is blocked in, which no event can reach.
    control: TcpStream,
    /// The terminal as it was, for a core that nothing can free.
    cooked: sys::Termios,
}

impl EscapeExit {
    /// The core says goodbye if it is listening. If it is stuck in a write to a
    /// server that stopped reading, ending the socket is the one thing that frees
    /// it, and is what `~.` is for. The queue may be full for the same reason. A
    /// core still here after that is printing to a stderr nobody reads, where no
    /// socket reaches it, so this thread ends the process as the core would have.
    fn disconnect(&self) {
        self.escaped.store(true, Ordering::SeqCst);
        let _ = self.events.try_send(Event::EscapeDisconnect);
        thread::sleep(FAREWELL_WRITE);
        let _ = self.control.shutdown(Shutdown::Both);
        thread::sleep(ESCAPE_LAST_RESORT);
        let _ = sys::set_terminal_attributes(io::stdin(), &self.cooked);
        set_terminal_raw(false);
        std::process::exit(EXIT_FAILURE);
    }
}

/// A raw terminal's stdin, read apart from the source that sends it. The source
/// waits for window and for a rekey's end, which a server can make for ever, and
/// `~.` has to be read all the same. So it is, until `ESCAPE_BACKLOG` chunks are
/// waiting for a window that does not open, as OpenSSH also stops reading.
struct EscapingStdin {
    chunks: Receiver<Vec<u8>>,
    pending: Vec<u8>,
}

impl EscapingStdin {
    fn spawn(mut filter: EscapeFilter, exit: EscapeExit) -> Self {
        let (sender, chunks) = mpsc::sync_channel(ESCAPE_BACKLOG);
        let starter = exit.events.clone();
        conn::start_thread(&starter, move || {
            let mut raw = [0u8; 4096];
            let mut input = Patient(io::stdin());
            loop {
                let got = match input.read(&mut raw) {
                    Ok(0) => return,
                    Ok(got) => got,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => return,
                };
                let mut out = Vec::with_capacity(got + 1);
                if filter.filter(raw.get(..got).unwrap_or_default(), &mut out) {
                    exit.disconnect();
                    return;
                }
                if !out.is_empty() && sender.send(out).is_err() {
                    return;
                }
            }
        });
        Self {
            chunks,
            pending: Vec::new(),
        }
    }
}

impl Read for EscapingStdin {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if !self.pending.is_empty() {
                let count = buffer.len().min(self.pending.len());
                let (head, _) = buffer.split_at_mut(count);
                head.copy_from_slice(self.pending.get(..count).unwrap_or_default());
                self.pending.drain(..count);
                return Ok(count);
            }
            let Ok(chunk) = self.chunks.recv() else {
                return Ok(0);
            };
            self.pending = chunk;
        }
    }
}

/// Everything after the transport: one session channel and the event loop.
/// Begins stdin once the channel is open and the terminal is settled: maximum
/// packet, its credit, and, if stdin's terminal went raw, what it was before.
type StdinStarter = Box<dyn FnOnce(usize, Arc<Credit>, Option<sys::Termios>) + Send>;

/// A request the server still owes an answer, in the order it answers them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    Pty,
    Session,
}

struct Session {
    peer_channel: Option<u32>,
    credit: Option<Arc<Credit>>,
    window: LocalWindow,
    stdout: Option<Sender<Vec<u8>>>,
    stderr: Option<Sender<Vec<u8>>>,
    handles: Vec<JoinHandle<()>>,
    exit_code: Option<i32>,
    want_tty: bool,
    /// Stdin is a terminal, to go raw once a PTY is granted and only then.
    stdin_is_tty: bool,
    guard: Option<TerminalGuard>,
    max_packet: u32,
    start_stdin: Option<StdinStarter>,
    /// `-n`: stdin is never read, so nothing else signals EOF.
    stdin_null: bool,
    command: Option<Vec<u8>>,
    env: Vec<(String, Vec<u8>)>,
    term: String,
    size: sys::Winsize,
    pending: VecDeque<Pending>,
    /// Stdout's consumer has gone, and the server has been told.
    stdout_broken: bool,
    logger: Logger,
}

/// Hands data to its sink, whose write will credit the window. False when there
/// is no such write coming, and the caller has to credit it.
fn forward(sink: Option<&Sender<Vec<u8>>>, data: &[u8]) -> bool {
    sink.is_some_and(|sink| sink.send(data.to_vec()).is_ok())
}

impl Session {
    fn open_message() -> Vec<u8> {
        let mut open = Writer::message(msg::CHANNEL_OPEN);
        open.str("session")
            .u32(SESSION_CHANNEL)
            .u32(CHANNEL_WINDOW)
            .u32(CHANNEL_MAX_PACKET);
        open.into_vec()
    }

    fn on_confirmation<W: Write, E: KexEnvironment>(
        &mut self,
        reader: &mut Reader<'_>,
        conn: &mut Conn<W, E>,
    ) -> Result<()> {
        let local = reader.u32()?;
        if local != SESSION_CHANNEL || self.peer_channel.is_some() {
            return Err(Error::protocol("unexpected channel confirmation"));
        }
        let peer = reader.u32()?;
        let window = reader.u32()?;
        let max_packet = reader.u32()?;
        self.peer_channel = Some(peer);
        self.max_packet = max_packet.min(CHANNEL_MAX_PACKET);
        let credit = conn.new_credit(window);
        self.credit = Some(credit.clone());
        for (name, value) in &self.env {
            let mut request = channel_request(peer, "env", false);
            request.str(name).string(value);
            conn.send(request.into_vec())?;
        }
        if self.want_tty {
            let mut request = channel_request(peer, "pty-req", true);
            request
                .str(&self.term)
                .u32(u32::from(self.size.columns))
                .u32(u32::from(self.size.rows))
                .u32(u32::from(self.size.width))
                .u32(u32::from(self.size.height));
            // Only TTY_OP_END: the server gives a new terminal its own defaults.
            request.string(&[0]);
            self.pending.push_back(Pending::Pty);
            conn.send(request.into_vec())?;
        }
        let request = match &self.command {
            Some(command) => {
                let mut request = channel_request(peer, "exec", true);
                request.string(command);
                request
            }
            None => channel_request(peer, "shell", true),
        };
        self.pending.push_back(Pending::Session);
        conn.send(request.into_vec())?;
        if self.stdin_null {
            let mut eof = Writer::message(msg::CHANNEL_EOF);
            eof.u32(peer);
            conn.send(eof.into_vec())?;
        }
        // A terminal is only settled by the answer to its request.
        if !self.want_tty {
            self.begin_stdin(None);
        }
        Ok(())
    }

    fn begin_stdin(&mut self, cooked: Option<sys::Termios>) {
        if let (Some(start), Some(credit)) = (self.start_stdin.take(), self.credit.clone()) {
            start(self.max_packet as usize, credit, cooked);
        }
    }

    /// The terminal goes raw only for a PTY that exists: a refused one leaves it
    /// cooked, and the session usable, as OpenSSH says it does.
    fn on_pty_reply(&mut self, granted: bool) {
        if granted {
            if self.stdin_is_tty {
                self.guard = TerminalGuard::enter();
            }
        } else {
            self.logger.error(&format!(
                "PTY allocation request failed on channel {SESSION_CHANNEL}"
            ));
        }
        self.begin_stdin(self.guard.as_ref().map(|guard| guard.saved));
    }

    /// `eow@openssh.com`, as OpenSSH sends it, for the program writing to a
    /// consumer that has gone. What is still coming is discarded and credited: the
    /// window is shared with stderr, and a server may finish what it was sending.
    fn on_stdout_broken<W: Write, E: KexEnvironment>(
        &mut self,
        conn: &mut Conn<W, E>,
    ) -> Result<()> {
        self.stdout_broken = true;
        if let Some(peer) = self.peer_channel {
            conn.send(channel_request(peer, "eow@openssh.com", false).into_vec())?;
        }
        Ok(())
    }

    fn on_exit_report(&mut self, report: ExitReport) {
        self.exit_code = Some(match report {
            ExitReport::Status(status) => status,
            ExitReport::Signal(signal) => {
                // What this client's own eow asked for is no news to anyone.
                if !(self.stdout_broken && signal == "PIPE") {
                    self.logger
                        .info(&format!("Remote command exited on signal {signal}"));
                }
                EXIT_FAILURE
            }
        });
    }

    /// Reopens the window as far as `bytes` allow.
    fn grant_window<W: Write, E: KexEnvironment>(
        &mut self,
        bytes: usize,
        conn: &mut Conn<W, E>,
    ) -> Result<()> {
        let bytes = u32::try_from(bytes).unwrap_or(u32::MAX);
        if let (Some(grant), Some(peer)) = (self.window.consumed(bytes), self.peer_channel) {
            let mut message = Writer::message(msg::CHANNEL_WINDOW_ADJUST);
            message.u32(peer).u32(grant);
            conn.send(message.into_vec())?;
        }
        Ok(())
    }

    /// Handles one connection-layer message. True when the session has ended.
    fn on_message<W: Write, E: KexEnvironment>(
        &mut self,
        payload: &[u8],
        seq: u32,
        conn: &mut Conn<W, E>,
    ) -> Result<bool> {
        let number = payload.first().copied().unwrap_or(0);
        let mut reader = Reader::new(payload.get(1..).unwrap_or_default());
        match number {
            msg::GLOBAL_REQUEST => {
                let _name = reader.string()?;
                if reader.bool()? {
                    conn.send(vec![msg::REQUEST_FAILURE])?;
                }
            }
            msg::REQUEST_SUCCESS | msg::REQUEST_FAILURE => {}
            msg::CHANNEL_OPEN_CONFIRMATION => self.on_confirmation(&mut reader, conn)?,
            msg::CHANNEL_OPEN_FAILURE => {
                let _ = reader.u32()?;
                let _ = reader.u32()?;
                let text = peer_text(reader.string()?);
                return Err(Error::refused(format!("channel open failed: {text}")));
            }
            msg::CHANNEL_OPEN => {
                let _ = reader.string()?;
                let peer = reader.u32()?;
                let mut failure = Writer::message(msg::CHANNEL_OPEN_FAILURE);
                failure
                    .u32(peer)
                    .u32(msg::OPEN_ADMINISTRATIVELY_PROHIBITED)
                    .str("td-ssh opens no channels for the server")
                    .str("");
                conn.send(failure.into_vec())?;
            }
            msg::CHANNEL_WINDOW_ADJUST => {
                let _ = reader.u32()?;
                let grant = reader.u32()?;
                if let Some(credit) = &self.credit {
                    credit.add(grant);
                }
            }
            msg::CHANNEL_DATA => {
                let _ = reader.u32()?;
                let data = reader.string()?;
                self.window.charge(data.len())?;
                if !forward(self.stdout.as_ref(), data) {
                    self.grant_window(data.len(), conn)?;
                }
            }
            msg::CHANNEL_EXTENDED_DATA => {
                let _ = reader.u32()?;
                let kind = reader.u32()?;
                let data = reader.string()?;
                self.window.charge(data.len())?;
                let sink = self
                    .stderr
                    .as_ref()
                    .filter(|_| kind == msg::EXTENDED_DATA_STDERR);
                if !forward(sink, data) {
                    self.grant_window(data.len(), conn)?;
                }
            }
            msg::CHANNEL_EOF => {
                self.stdout = None;
                self.stderr = None;
            }
            msg::CHANNEL_REQUEST => {
                let _ = reader.u32()?;
                let request = reader.str()?;
                let want_reply = reader.bool()?;
                if let Some(report) = ExitReport::read(request, &mut reader)? {
                    self.on_exit_report(report);
                }
                if want_reply {
                    if let Some(peer) = self.peer_channel {
                        let mut reply = Writer::message(msg::CHANNEL_FAILURE);
                        reply.u32(peer);
                        conn.send(reply.into_vec())?;
                    }
                }
            }
            msg::CHANNEL_SUCCESS => {
                if self.pending.pop_front() == Some(Pending::Pty) {
                    self.on_pty_reply(true);
                }
            }
            msg::CHANNEL_FAILURE => match self.pending.pop_front() {
                Some(Pending::Pty) => self.on_pty_reply(false),
                Some(Pending::Session) => {
                    return Err(Error::refused("the server refused the session request"));
                }
                None => {}
            },
            msg::CHANNEL_CLOSE => {
                // The server has ended the session, so a reply it cannot take, as when
                // its end of connection came straight after, changes nothing.
                if let Some(peer) = self.peer_channel {
                    let mut close = Writer::message(msg::CHANNEL_CLOSE);
                    close.u32(peer);
                    let _ = conn.send(close.into_vec());
                }
                return Ok(true);
            }
            _ => {
                let mut reply = Writer::message(msg::UNIMPLEMENTED);
                reply.u32(seq);
                conn.send(reply.into_vec())?;
            }
        }
        Ok(false)
    }
}

/// How a command ended, as the server said in a channel request.
enum ExitReport {
    Status(i32),
    Signal(String),
}

impl ExitReport {
    /// `reader` is past the request's name and `want_reply`.
    fn read(request: &str, reader: &mut Reader<'_>) -> Result<Option<Self>> {
        Ok(match request {
            "exit-status" => Some(Self::Status(
                i32::try_from(reader.u32()? & 0xff).unwrap_or(EXIT_FAILURE),
            )),
            "exit-signal" => Some(Self::Signal(peer_text(reader.string()?))),
            _ => None,
        })
    }

    /// What a message the core never got to says, if it says this.
    fn in_packet(payload: &[u8]) -> Option<Self> {
        let (&number, rest) = payload.split_first()?;
        if number != msg::CHANNEL_REQUEST {
            return None;
        }
        let mut reader = Reader::new(rest);
        reader.u32().ok()?;
        let request = reader.str().ok()?;
        reader.bool().ok()?;
        Self::read(request, &mut reader).ok()?
    }
}

/// How long `~.` lets the output land, from whenever it comes.
struct EscapeWait<'a> {
    escaped: &'a AtomicBool,
    patience: Duration,
}

/// Puts the terminal back, then waits for the output already received to land.
/// The terminal comes first because a stalled consumer can hold that wait open for
/// as long as it likes, unless `~.` is pressed, which it then outlasts by only
/// `patience`. A core stuck elsewhere is `EscapeExit::disconnect`'s to end.
fn finish_output(
    restore: impl FnOnce(),
    handles: Vec<JoinHandle<()>>,
    receiver: &EventReceiver,
    escape: &EscapeWait<'_>,
    observe: &mut impl FnMut(Event),
) {
    restore();
    drain_while_joining(handles, receiver, escape, observe);
}

/// A sink sends an event per message and this is the only thread draining them,
/// so a plain join would deadlock on a full queue, and dropping the receiver
/// would make the sinks return with output unwritten. Only the sinks ending ends
/// this: a peer still sending would keep the queue itself alive for ever. So the
/// look at what they left behind is bounded too. `observe` sees every event taken.
fn drain_while_joining(
    handles: Vec<JoinHandle<()>>,
    receiver: &EventReceiver,
    escape: &EscapeWait<'_>,
    observe: &mut impl FnMut(Event),
) {
    let mut give_up = None;
    for handle in handles {
        while !handle.is_finished() {
            if escape.escaped.load(Ordering::SeqCst) {
                let at = *give_up.get_or_insert_with(|| Instant::now() + escape.patience);
                if Instant::now() >= at {
                    return;
                }
            }
            match receiver.recv_timeout(DRAIN_POLL) {
                Ok(event) => observe(event),
                Err(RecvTimeoutError::Disconnected) => thread::sleep(DRAIN_POLL),
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        let _ = handle.join();
    }
    for _ in 0..conn::EVENT_QUEUE {
        match receiver.recv_timeout(Duration::ZERO) {
            Ok(event) => observe(event),
            Err(_) => return,
        }
    }
}

fn channel_request(peer: u32, name: &str, want_reply: bool) -> Writer {
    let mut request = Writer::message(msg::CHANNEL_REQUEST);
    request.u32(peer).str(name).bool(want_reply);
    request
}

/// The name and home `/etc/passwd` gives a uid.
fn passwd_entry(table: &str, uid: u32) -> Option<(String, String)> {
    table.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next().filter(|name| !name.is_empty())?;
        let listed: u32 = fields.nth(1)?.parse().ok()?;
        let home = fields.nth(2)?;
        (listed == uid).then(|| (name.to_string(), home.to_string()))
    })
}

/// The real uid's entry. What is unavailable is only a problem if it is needed.
fn caller_passwd_entry() -> Option<(String, String)> {
    let table = fs::read_to_string("/etc/passwd").ok()?;
    passwd_entry(&table, caller_uid().ok()?)
}

/// The user's home directory and name, from the environment first and otherwise
/// the real uid's entry: td-agent's environment holds neither. No name at all is
/// an error once one is needed, not a login attempt as somebody called `unknown`.
fn local_identity(
    variable: impl Fn(&str) -> Option<OsString>,
    entry: impl FnOnce() -> Option<(String, String)>,
) -> (PathBuf, Option<String>) {
    let set = |name: &str| variable(name).filter(|value| !value.is_empty());
    let home = set("HOME");
    let user = set("USER")
        .or_else(|| set("LOGNAME"))
        .and_then(|user| user.into_string().ok());
    if let (Some(home), Some(user)) = (&home, &user) {
        return (PathBuf::from(home), Some(user.clone()));
    }
    let (entry_user, entry_home) = entry().unzip();
    let home = home
        .or(entry_home
            .filter(|home| !home.is_empty())
            .map(OsString::from))
        .map_or_else(|| PathBuf::from("/"), PathBuf::from);
    (home, user.or(entry_user))
}

/// `-F FILE` alone, `-F none` for nothing at all, else the user's then the
/// system's file.
fn read_configuration(
    options: &mut Options,
    config_file: Option<&Path>,
    host: &str,
    home: &Path,
) -> Result<()> {
    match config_file {
        Some(path) if path.as_os_str() == "none" => Ok(()),
        Some(path) => options.read_file(path, host, Origin::Given),
        None => {
            let user_config = home.join(".ssh/config");
            if user_config.exists() {
                options.read_file(&user_config, host, Origin::User)?;
            }
            let system = Path::new("/etc/ssh/ssh_config");
            if system.exists() {
                options.read_file(system, host, Origin::Given)?;
            }
            Ok(())
        }
    }
}

/// The files a `UserKnownHostsFile` or `GlobalKnownHostsFile` names. `none` is
/// no file, so nothing is read or recorded, not a file called `none`.
fn known_hosts_files(
    configured: Option<&Vec<String>>,
    defaults: &[&Path],
    expand: impl Fn(&str) -> Result<PathBuf>,
) -> Result<Vec<PathBuf>> {
    match configured {
        Some(files) => files
            .iter()
            .filter(|file| !file.eq_ignore_ascii_case("none"))
            .map(|file| expand(file))
            .collect(),
        None => Ok(defaults.iter().map(|path| path.to_path_buf()).collect()),
    }
}

/// A connection through authentication, ready for the session stage.
struct Established {
    reader: PacketReader<SetupStream>,
    writer: PacketWriter<TcpStream>,
    kex: Kex,
    env: ClientEnv,
}

fn establish(
    stream: TcpStream,
    mut env: ClientEnv,
    algorithms: Vec<&'static str>,
    login: &Login<'_>,
    limits: SetupLimits,
) -> Result<Established> {
    let write_half = stream.try_clone()?;
    let mut stream = SetupStream {
        stream,
        clock: limits.clock,
    };
    let server_version = exchange_versions(&mut stream, true)?;
    login
        .logger
        .debug(&format!("Remote protocol version: {server_version}"));
    let mut kex = Kex::new(Role::Client, IDENTIFICATION, &server_version, algorithms);
    let mut reader = PacketReader::new(stream);
    let mut writer = PacketWriter::new(write_half);
    let session_id = conn::initial_kex(&mut reader, &mut writer, &mut kex, &mut env)?;
    if let Some(name) = kex.negotiated() {
        login.logger.debug(&format!("kex: algorithm: {name}"));
    }
    let setup = reader.get_mut();
    setup.clock.stop();
    setup.stream.set_read_timeout(limits.auth_read)?;
    let mut link = AuthLink {
        reader: &mut reader,
        writer: &mut writer,
        kex: &mut kex,
        env: &mut env,
    };
    userauth_service(&mut link)?;
    authenticate(&mut link, &session_id, login)?;
    Ok(Established {
        reader,
        writer,
        kex,
        env,
    })
}

/// A command is one packet, so this is what stands between a long one and a
/// failure that names no cause.
fn check_command_length(command: &[u8]) -> Result<()> {
    if command.len() > MAX_REMOTE_COMMAND {
        return Err(Error::refused(format!(
            "remote command is too long: {} bytes, and one SSH packet carries at most {MAX_REMOTE_COMMAND}",
            command.len()
        )));
    }
    Ok(())
}

pub fn run(mut invocation: Invocation) -> Result<i32> {
    if let Some(command) = &invocation.command {
        check_command_length(command)?;
    }
    let (_, original_host, _) = lowered_destination(&invocation.destination)?;
    let (home, local_user) = local_identity(|name| std::env::var_os(name), caller_passwd_entry);
    let no_user = || match caller_uid() {
        Ok(uid) => Error::refused(format!("No user exists for uid {uid}")),
        Err(error) => error,
    };
    let local_name = local_user.as_deref().unwrap_or_default().to_string();
    let mut options = std::mem::take(&mut invocation.options);
    read_configuration(
        &mut options,
        invocation.config_file.as_deref(),
        &original_host,
        &home,
    )?;
    options.validate()?;

    // Whatever a `HostName` supplied is recorded as much as a destination is.
    let host = checked_host(&target_host(&options, &original_host))?;
    let port = options.port.unwrap_or(22);
    let user = match options.user.clone().or(local_user) {
        Some(user) => user,
        None => return Err(no_user()),
    };
    if invocation.dump_config {
        return Ok(dump_config(&options, &host, &user, port));
    }

    let level = if invocation.quiet {
        LogLevel::Quiet
    } else if invocation.verbosity > 0 {
        LogLevel::Debug
    } else {
        options.log_level.unwrap_or(LogLevel::Info)
    };
    let logger = Logger { level };

    let batch = options.batch_mode.unwrap_or(false);
    let expand = |value: &str| {
        expand_path(
            value,
            &home,
            &host,
            &original_host,
            &user,
            &local_name,
            port,
        )
    };
    let user_files = known_hosts_files(
        options.user_known_hosts.as_ref(),
        &[
            home.join(".ssh/known_hosts").as_path(),
            home.join(".ssh/known_hosts2").as_path(),
        ],
        expand,
    )?;
    let global_files = known_hosts_files(
        options.global_known_hosts.as_ref(),
        &[
            Path::new("/etc/ssh/ssh_known_hosts"),
            Path::new("/etc/ssh/ssh_known_hosts2"),
        ],
        expand,
    )?;
    let mut identity_paths: Vec<PathBuf> = Vec::new();
    for value in invocation
        .identity_args
        .iter()
        .chain(&options.identity_files)
    {
        if value.eq_ignore_ascii_case("none") {
            continue;
        }
        identity_paths.push(expand(value)?);
    }
    let explicit_identities = !identity_paths.is_empty()
        || options
            .identity_files
            .iter()
            .any(|value| value.eq_ignore_ascii_case("none"));
    if !explicit_identities {
        identity_paths.push(home.join(".ssh/id_ed25519"));
    }
    let identities: Vec<Identity> = identity_paths
        .iter()
        .filter_map(|path| load_identity(path, &logger))
        .collect();

    let timeout = options
        .connect_timeout
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs);
    let stream = connect(&host, port, invocation.ipv6, timeout)?;
    let _ = stream.set_nodelay(true);
    apply_socket_limits(&stream, &options)?;
    // Socket options, not a descriptor's: this handle reaches the live socket
    // whatever owns the others by then.
    let control = stream.try_clone()?;
    let clock = SetupClock::until(connect_limit(&options));
    let peer_address = stream
        .peer_addr()
        .map(|peer| peer.ip())
        .unwrap_or(IpAddr::from([0, 0, 0, 0]));

    let check = HostCheck {
        name: known_hosts::host_key_name(options.host_key_alias.as_deref(), &host, port),
        display_host: host.clone(),
        address: peer_address.to_string(),
        user_files,
        global_files,
        strict: options.strict.unwrap_or(Strict::Ask),
        hash: options.hash_known_hosts.unwrap_or(false),
        batch,
        logger: Logger { level },
        accepted: None,
        clock: clock.clone(),
    };
    let env = ClientEnv {
        random: RandomSource(File::open("/dev/urandom")?),
        check,
        pinned: None,
    };
    let login = Login {
        user: &user,
        identities: &identities,
        prompts: PASSPHRASE_TRIES,
        batch,
        logger: &logger,
    };
    let algorithms = options.kex.clone().unwrap_or_else(|| DEFAULT_KEX.to_vec());
    let limits = SetupLimits {
        clock,
        auth_read: auth_read_limit(&options),
    };
    let established =
        establish(stream, env, algorithms, &login, limits).map_err(|error| match error {
            Error::Refused(text) if text.starts_with("Permission denied") => {
                Error::Refused(format!("{user}@{host}: {text}"))
            }
            error if is_timeout(&error) => Error::Refused(timeout_message(&invocation.destination)),
            other => other,
        })?;
    logger.debug("Authenticated.");
    // The private keys have done all they will this connection.
    drop(identities);
    control.set_read_timeout(None)?;

    session_loop(established, control, &invocation, &options, logger)
}

fn dump_config(options: &Options, host: &str, user: &str, port: u16) -> i32 {
    let mut out = format!("user {user}\nhostname {host}\nport {port}\n");
    for name in options.kex.as_deref().unwrap_or(DEFAULT_KEX) {
        out.push_str(&format!("kexalgorithms {name}\n"));
    }
    out.push_str(&format!(
        "ciphers {CIPHER}\nhostkeyalgorithms {HOST_KEY_ALGORITHM}\n"
    ));
    match primary_output(&out) {
        Ok(()) => 0,
        Err(_) => EXIT_FAILURE,
    }
}

/// What ended a session that nothing failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    ChannelClosed,
    /// The user's `~.`, which reads as no remote success whatever came before.
    Escaped,
}

fn session_status(ending: Ending, exit_code: Option<i32>) -> i32 {
    match ending {
        Ending::Escaped => EXIT_FAILURE,
        Ending::ChannelClosed => exit_code.unwrap_or(EXIT_FAILURE),
    }
}

/// The environment `SendEnv`'s patterns select, of valid names only.
fn selected_env(
    patterns: &[String],
    environment: impl Iterator<Item = (OsString, OsString)>,
) -> Vec<(String, Vec<u8>)> {
    environment
        .filter_map(|(name, value)| {
            let name = name.into_string().ok()?;
            let wanted = valid_env_name(&name) && patterns.iter().any(|p| glob(p, &name));
            wanted.then(|| (name, value.into_vec()))
        })
        .collect()
}

/// As OpenSSH's check: anything else a server's shell could not name anyway.
fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn session_loop(
    established: Established,
    control: TcpStream,
    invocation: &Invocation,
    options: &Options,
    logger: Logger,
) -> Result<i32> {
    let Established {
        reader,
        writer,
        kex,
        env,
    } = established;
    let (events, receiver) = conn::event_channel();
    let ending = control.try_clone()?;
    let rx_keys = conn::spawn_reader(reader, events.clone(), move || {
        let _ = ending.shutdown(Shutdown::Both);
    });
    let mut conn = Conn::new(writer, rx_keys, kex, env, events.flow().clone());
    if let Some(limit) = options.rekey_limit {
        conn.limit_rekey_bytes(limit);
    }

    let stdin_null = invocation.stdin_null;
    let no_session = invocation.no_session;
    let stdin_is_tty = !stdin_null && sys::is_terminal(io::stdin());
    let tty = decide_tty(invocation, options, stdin_is_tty);
    let want_tty = !no_session && tty.request;
    if !no_session && tty.no_terminal {
        logger.info("Pseudo-terminal will not be allocated because stdin is not a terminal.");
    }
    let size = sys::window_size(io::stdin()).unwrap_or(sys::Winsize {
        rows: 24,
        columns: 80,
        width: 0,
        height: 0,
    });
    let window_stop = Arc::new(AtomicBool::new(false));
    let escaped = Arc::new(AtomicBool::new(false));

    let start_stdin: Option<StdinStarter> = if stdin_null || no_session {
        None
    } else {
        let events = events.clone();
        let window_stop = window_stop.clone();
        let escaped = escaped.clone();
        let control = control.try_clone()?;
        Some(Box::new(move |max_packet, credit, cooked| {
            if cooked.is_some() {
                spawn_window_watcher(events.clone(), size, window_stop);
            }
            match cooked {
                Some(cooked) => {
                    let exit = EscapeExit {
                        events: events.clone(),
                        escaped,
                        control,
                        cooked,
                    };
                    let input = EscapingStdin::spawn(EscapeFilter::new(DEFAULT_ESCAPE), exit);
                    conn::spawn_source(input, SESSION_CHANNEL, false, max_packet, credit, events);
                }
                None => {
                    conn::spawn_source(
                        Patient(io::stdin()),
                        SESSION_CHANNEL,
                        false,
                        max_packet,
                        credit,
                        events,
                    );
                }
            }
        }))
    };
    let (stdout, stdout_handle) = conn::spawn_sink(
        Patient(io::stdout()),
        SESSION_CHANNEL,
        WhenBroken::Report,
        events.clone(),
    );
    let (stderr, stderr_handle) = conn::spawn_sink(
        Patient(io::stderr()),
        SESSION_CHANNEL,
        WhenBroken::Discard,
        events.clone(),
    );
    let send_env = selected_env(&options.send_env, std::env::vars_os());

    let mut session = Session {
        peer_channel: None,
        credit: None,
        window: LocalWindow::default(),
        stdout: Some(stdout),
        stderr: Some(stderr),
        handles: [stdout_handle, stderr_handle]
            .into_iter()
            .flatten()
            .collect(),
        exit_code: None,
        want_tty,
        stdin_is_tty,
        guard: None,
        max_packet: CHANNEL_MAX_PACKET,
        start_stdin,
        stdin_null,
        command: invocation.command.clone(),
        env: send_env,
        term: std::env::var("TERM").unwrap_or_else(|_| "dumb".to_string()),
        size,
        pending: VecDeque::new(),
        stdout_broken: false,
        logger,
    };

    if !no_session {
        conn.send(Session::open_message())?;
    }
    if options.server_alive_interval.is_some_and(|value| value > 0) {
        spawn_ticker(events.clone(), TICK)?;
    }
    drop(events);

    let alive_interval = options
        .server_alive_interval
        .filter(|value| *value > 0)
        .map(Duration::from_secs);
    let alive_max = options
        .server_alive_count_max
        .unwrap_or(DEFAULT_ALIVE_COUNT);
    let mut alive_misses = 0u32;
    let mut last_probe = Instant::now();
    let mut last_seen = conn.arrivals();

    let outcome = loop {
        let Ok(event) = receiver.recv() else {
            break Err(Error::protocol("the connection loop lost its events"));
        };
        match event {
            Event::Packet { payload, seq } => match conn.on_packet(payload) {
                Ok(Some(message)) => match session.on_message(&message, seq, &mut conn) {
                    Ok(true) => break Ok(Ending::ChannelClosed),
                    Ok(false) => {}
                    Err(error) => break Err(error),
                },
                Ok(None) => {}
                Err(error) => break Err(error),
            },
            Event::TransportEnded(error) => break Err(error),
            Event::Output { data, .. } => {
                if let Some(peer) = session.peer_channel {
                    let mut message = Writer::message(msg::CHANNEL_DATA);
                    message.u32(peer).string(&data);
                    if let Err(error) = conn.send(message.into_vec()) {
                        break Err(error);
                    }
                }
            }
            Event::OutputEof { .. } => {
                if let Some(peer) = session.peer_channel {
                    let mut message = Writer::message(msg::CHANNEL_EOF);
                    message.u32(peer);
                    if let Err(error) = conn.send(message.into_vec()) {
                        break Err(error);
                    }
                }
            }
            Event::Consumed { bytes, .. } => {
                if let Err(error) = session.grant_window(bytes as usize, &mut conn) {
                    break Err(error);
                }
            }
            Event::WindowChanged {
                columns,
                rows,
                width,
                height,
            } => {
                if let Some(peer) = session.peer_channel {
                    let mut request = channel_request(peer, "window-change", false);
                    request.u32(columns).u32(rows).u32(width).u32(height);
                    if let Err(error) = conn.send(request.into_vec()) {
                        break Err(error);
                    }
                }
            }
            Event::Tick => {
                let Some(interval) = alive_interval else {
                    continue;
                };
                if conn.arrivals() != last_seen {
                    last_seen = conn.arrivals();
                    alive_misses = 0;
                    last_probe = Instant::now();
                }
                if last_probe.elapsed() >= interval {
                    last_probe = Instant::now();
                    alive_misses = alive_misses.saturating_add(1);
                    if alive_misses > alive_max {
                        session
                            .logger
                            .info(&timeout_message(&invocation.destination));
                        break Err(Error::Disconnected(String::new()));
                    }
                    let mut probe = Writer::message(msg::GLOBAL_REQUEST);
                    probe.str("keepalive@openssh.com").bool(true);
                    if let Err(error) = conn.send(probe.into_vec()) {
                        break Err(error);
                    }
                }
            }
            Event::SinkBroken { .. } => {
                if let Err(error) = session.on_stdout_broken(&mut conn) {
                    break Err(error);
                }
            }
            Event::EscapeDisconnect => break Ok(Ending::Escaped),
            Event::ChildExited { .. } => {}
        }
    };
    window_stop.store(true, Ordering::Relaxed);
    if let Some(credit) = &session.credit {
        credit.close();
    }
    session.stdout = None;
    session.stderr = None;
    let handles = std::mem::take(&mut session.handles);
    // A connection that ended under a blocked write left its last words in the queue.
    let lost = outcome.is_err();
    // One, not a list: a peer can keep sending for as long as a stalled sink holds
    // this wait open.
    let mut report = None;
    let mut dropped_output = false;
    let mut closed = false;
    let mut transport_end = None;
    finish_output(
        || {
            if let Some(guard) = &mut session.guard {
                guard.restore();
            }
        },
        handles,
        &receiver,
        &EscapeWait {
            escaped: &escaped,
            patience: FAREWELL_WRITE,
        },
        &mut |event| match event {
            Event::Packet { payload, .. } if lost => {
                dropped_output |= matches!(
                    payload.first(),
                    Some(&(msg::CHANNEL_DATA | msg::CHANNEL_EXTENDED_DATA))
                );
                closed |= payload.first() == Some(&msg::CHANNEL_CLOSE);
                if let Some(found) = ExitReport::in_packet(&payload) {
                    report = Some(found);
                }
            }
            Event::TransportEnded(error) if lost => transport_end = Some(error),
            _ => {}
        },
    );
    // The sinks are closed by now, so output in that queue has nowhere to go, and a
    // success on top of it would say the command's output had all arrived.
    if dropped_output {
        session.exit_code = None;
        if !escaped.load(Ordering::SeqCst) {
            session
                .logger
                .info("Output after the connection failed was lost");
        }
    } else if let Some(report) = report {
        session.on_exit_report(report);
    }
    session
        .logger
        .debug(&format!("rekeyed {} times", conn.completed_rekeys()));
    // Whatever the wait for the core ended in, this is why it ended.
    let outcome = if escaped.load(Ordering::SeqCst) {
        Ok(Ending::Escaped)
    } else {
        // The write failing was the reader closing the socket on this, which may have
        // been a server that had already closed the session and said all it had.
        match (outcome, transport_end) {
            (Err(Error::Io(_)), _) if closed && !dropped_output => Ok(Ending::ChannelClosed),
            (Err(Error::Io(_)), Some(ended)) => Err(ended),
            (outcome, _) => outcome,
        }
    };
    match outcome {
        Ok(ending) => {
            let _ = control.set_write_timeout(Some(FAREWELL_WRITE));
            conn.disconnect(msg::DISCONNECT_BY_APPLICATION, "disconnected by user");
            Ok(session_status(ending, session.exit_code))
        }
        Err(error) => lost_connection(
            error,
            session.exit_code,
            &session.logger,
            &invocation.destination,
        ),
    }
}

/// The connection ended unasked. A command whose status has arrived has finished,
/// whatever then happened to the connection, and what it said stands.
fn lost_connection(
    error: Error,
    exit_code: Option<i32>,
    logger: &Logger,
    destination: &str,
) -> Result<i32> {
    let status = exit_code.unwrap_or(EXIT_FAILURE);
    match error {
        Error::Disconnected(text) if text.is_empty() => Ok(status),
        Error::Io(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
            logger.info("Connection closed by remote host");
            Ok(status)
        }
        error if is_timeout(&error) => {
            logger.info(&timeout_message(destination));
            Ok(status)
        }
        error @ (Error::Io(_) | Error::Disconnected(_)) if exit_code.is_some() => {
            logger.info(&error.to_string());
            Ok(status)
        }
        error => Err(error),
    }
}

/// Whether to ask for a PTY, and whether one was asked for that stdin cannot
/// back, which OpenSSH says aloud.
#[derive(Debug, PartialEq, Eq)]
struct TtyChoice {
    request: bool,
    no_terminal: bool,
}

/// `-t`/`-T`, then `RequestTTY`, against what stdin is, as OpenSSH decides. Only
/// `-tt` and `force` ask without a terminal behind it. `options` are the merged ones:
/// `run` has taken the invocation's own by then.
fn decide_tty(invocation: &Invocation, options: &Options, stdin_is_tty: bool) -> TtyChoice {
    let wanted = TtyChoice {
        request: stdin_is_tty,
        no_terminal: !stdin_is_tty,
    };
    let settled = |request: bool| TtyChoice {
        request,
        no_terminal: false,
    };
    if invocation.no_tty {
        return settled(false);
    }
    if invocation.force_tty >= 2 {
        return settled(true);
    }
    if invocation.force_tty == 1 {
        return wanted;
    }
    match options.request_tty.unwrap_or(RequestTty::Auto) {
        RequestTty::No => settled(false),
        RequestTty::Force => settled(true),
        RequestTty::Yes => wanted,
        RequestTty::Auto => settled(invocation.command.is_none() && stdin_is_tty),
    }
}

fn spawn_ticker(events: conn::EventSender, every: Duration) -> io::Result<()> {
    thread::Builder::new().spawn(move || loop {
        thread::sleep(every);
        if events.send(Event::Tick).is_err() {
            return;
        }
    })?;
    Ok(())
}

/// A polling thread rather than a signal handler: no `sigaction` surface. With
/// no thread to be had, the size just stays what the server was first told.
fn spawn_window_watcher(events: conn::EventSender, initial: sys::Winsize, stop: Arc<AtomicBool>) {
    let _ = thread::Builder::new().spawn(move || {
        let stdin = io::stdin();
        let fd = stdin.as_fd();
        let mut last = initial;
        while !stop.load(Ordering::Relaxed) {
            thread::sleep(WINDOW_POLL);
            let Ok(size) = sys::window_size(fd) else {
                continue;
            };
            if size != last {
                last = size;
                let event = Event::WindowChanged {
                    columns: u32::from(size.columns),
                    rows: u32::from(size.rows),
                    width: u32::from(size.width),
                    height: u32::from(size.height),
                };
                if events.send(event).is_err() {
                    return;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;
    use std::sync::Mutex;
    use td_primitives::curve25519::ed25519;

    /// How long a test waits for what should end at once; a hang is a failure.
    const LIMIT: Duration = Duration::from_secs(20);

    fn strings(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn parsed(args: &[&str]) -> Invocation {
        match parse_args(&strings(args)).unwrap() {
            Parsed::Run(invocation) => *invocation,
            Parsed::PrintAndExit(_) => panic!("expected a run"),
        }
    }

    fn quiet() -> Logger {
        Logger {
            level: LogLevel::Quiet,
        }
    }

    #[test]
    fn git_style_invocations_parse() {
        let invocation = parsed(&[
            "-o",
            "SendEnv=GIT_PROTOCOL",
            "-p",
            "2222",
            "git@example",
            "git-upload-pack 'x.git'",
        ]);
        assert_eq!(invocation.options.port, Some(2222));
        assert_eq!(invocation.destination, "git@example");
        assert_eq!(
            invocation.command.as_deref(),
            Some(&b"git-upload-pack 'x.git'"[..])
        );
        assert_eq!(invocation.options.send_env, ["GIT_PROTOCOL"]);
    }

    /// Each pinned from stock `ssh -G`, whose rule is the first obtained value.
    #[test]
    fn the_first_user_and_port_given_win_in_command_line_order() {
        for (args, user, port) in [
            (&["-p", "1", "-p", "2", "h"][..], None, Some(1)),
            (&["-p", "1", "h", "-p", "2"], None, Some(1)),
            (&["-o", "Port=1", "-p", "2", "h"], None, Some(1)),
            (&["-p", "2", "-o", "Port=1", "h"], None, Some(2)),
            (&["-o", "Port=1", "-o", "Port=2", "h"], None, Some(1)),
            // A later one is not even parsed.
            (&["-p", "1", "-p", "bad", "h"], None, Some(1)),
            (&["-o", "Port=1", "-p", "bad", "h"], None, Some(1)),
            (&["-l", "a", "-l", "b", "h"], Some("a"), None),
            (&["-o", "User=a", "-l", "b", "h"], Some("a"), None),
            (&["-l", "b", "-o", "User=a", "h"], Some("b"), None),
            // The destination's own count where it stands.
            (&["-l", "b", "d@h"], Some("b"), None),
            (&["d@h", "-l", "b"], Some("d"), None),
            (&["-o", "User=c", "d@h"], Some("c"), None),
            (&["d@h", "-o", "User=c"], Some("d"), None),
            (&["-p", "6", "ssh://d@h:5"], Some("d"), Some(6)),
            (&["ssh://d@h:5", "-p", "6"], Some("d"), Some(5)),
            (&["-l", "b", "ssh://d@h:5"], Some("b"), Some(5)),
            (&["ssh://d@h:5", "-l", "b"], Some("d"), Some(5)),
            (&["ssh://h:5", "-o", "Port=6"], None, Some(5)),
            (&["-o", "Port=6", "ssh://h:5"], None, Some(6)),
        ] {
            let invocation = parsed(args);
            assert_eq!(invocation.options.user.as_deref(), user, "{args:?}");
            assert_eq!(invocation.options.port, port, "{args:?}");
        }
        assert!(parse_args(&["-p".into(), "bad".into(), "h".into()]).is_err());
    }

    /// The exception, as stock: the last `-F`.
    #[test]
    fn the_last_config_file_wins() {
        let invocation = parsed(&["-F", "one", "-F", "two", "h"]);
        assert_eq!(invocation.config_file, Some(PathBuf::from("two")));
    }

    #[test]
    fn options_may_follow_the_destination_and_stop_at_the_command() {
        let invocation = parsed(&["host", "-l", "user", "cmd", "-x", "arg"]);
        assert_eq!(invocation.options.user.as_deref(), Some("user"));
        assert_eq!(invocation.command.as_deref(), Some(&b"cmd -x arg"[..]));
    }

    #[test]
    fn a_remote_command_is_carried_as_bytes_and_the_rest_must_be_text() {
        let bytes = |raw: &[u8]| OsString::from_vec(raw.to_vec());
        let mut args = strings(&["h"]);
        args.push(bytes(b"\xff\xfe"));
        args.push(bytes(b"-\xc3"));
        let Parsed::Run(invocation) = parse_args(&args).unwrap() else {
            panic!("expected a run");
        };
        assert_eq!(invocation.command.as_deref(), Some(&b"\xff\xfe -\xc3"[..]));
        // After `--`, likewise.
        let mut args = strings(&["--", "h"]);
        args.push(bytes(b"\xff"));
        let Parsed::Run(invocation) = parse_args(&args).unwrap() else {
            panic!("expected a run");
        };
        assert_eq!(invocation.command.as_deref(), Some(&b"\xff"[..]));
        // An option, its value and the destination have to be text.
        for raw in [
            vec![bytes(b"\xff")],
            vec![bytes(b"-p\xff"), bytes(b"h")],
            vec![OsString::from("-l"), bytes(b"\xff"), OsString::from("h")],
        ] {
            let error = parse_args(&raw).err().unwrap().to_string();
            assert!(error.contains("not UTF-8"), "{error}");
        }
    }

    #[test]
    fn attached_values_clusters_and_no_ops_parse() {
        let invocation = parsed(&["-Tqax", "-p22", "-oBatchMode=yes", "-Fconf", "-iid", "h"]);
        assert!(invocation.no_tty && invocation.quiet);
        assert_eq!(invocation.options.port, Some(22));
        assert_eq!(invocation.options.batch_mode, Some(true));
        assert_eq!(invocation.config_file, Some(PathBuf::from("conf")));
        assert_eq!(invocation.identity_args, ["id"]);
    }

    #[test]
    fn a_flag_td_ssh_does_not_have_is_refused() {
        for flag in [
            "-L", "-R", "-D", "-A", "-J", "-W", "-X", "-Y", "-C", "-Q", "-k", "-e", "-Z",
        ] {
            let error = parse_args(&strings(&[flag, "h"]))
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("unknown option"), "{flag}: {error}");
        }
    }

    #[test]
    fn the_last_address_family_wins_and_a_is_forward_agent_no() {
        assert_eq!(parsed(&["h"]).ipv6, None);
        assert_eq!(parsed(&["-4", "-6", "h"]).ipv6, Some(true));
        assert_eq!(parsed(&["-6", "-4", "h"]).ipv6, Some(false));
        let mut invocation = parsed(&["-a", "h"]);
        invocation
            .options
            .read_text("ForwardAgent yes\n", "h", "test")
            .unwrap();
        assert!(Options::default()
            .read_text("ForwardAgent yes\n", "h", "test")
            .is_err());
    }

    #[test]
    fn t_and_big_t_override_request_tty() {
        let with = |args: &[&str], request: &str| {
            let mut invocation = parsed(args);
            invocation
                .options
                .read_text(&format!("RequestTTY {request}\n"), "h", "test")
                .unwrap();
            decide_tty(&invocation, &invocation.options, false).request
        };
        assert!(with(&["h"], "force"));
        assert!(!with(&["-T", "h"], "force"));
        assert!(!with(&["h"], "no"));
        assert!(with(&["-tt", "h"], "no"));
    }

    #[test]
    fn no_port_is_zero_on_the_command_line_or_in_a_url() {
        for args in [&["-p", "0", "h"][..], &["-p0", "h"], &["-o", "Port=0", "h"]] {
            assert!(parse_args(&strings(args)).is_err(), "{args:?}");
        }
        assert!(parse_destination("ssh://h:0").is_err());
        assert_eq!(parse_destination("ssh://h:1").unwrap().2, Some(1));
    }

    #[test]
    fn destinations_parse_and_a_leading_dash_is_refused() {
        assert_eq!(
            parse_destination("u@h").unwrap(),
            (Some("u".into()), "h".into(), None)
        );
        assert_eq!(
            parse_destination("ssh://u@h:2222/").unwrap(),
            (Some("u".into()), "h".into(), Some(2222))
        );
        assert_eq!(
            parse_destination("ssh://[::1]:2222").unwrap(),
            (None, "::1".into(), Some(2222))
        );
        assert_eq!(parse_destination("h").unwrap(), (None, "h".into(), None));
        // A leading dash reaches here only through user@-host or a URL.
        assert!(parse_destination("u@-oProxyCommand=x").is_err());
        assert!(parse_destination("ssh://-oProxyCommand=x").is_err());
        assert!(parse_destination("h ost").is_err());
        assert!(parse_destination("").is_err());
    }

    #[test]
    fn a_hashed_pin_holds_whatever_case_the_host_is_typed_in() {
        let (user, host, _) = lowered_destination("MixedUser@HoSt.Example").unwrap();
        // Only the host folds: an account name is case-sensitive.
        assert_eq!(user.as_deref(), Some("MixedUser"));
        assert_eq!(host, "host.example");
        let line = known_hosts::new_line("host.example", "KEY", Some(&[7; 20]));
        assert_eq!(known_hosts::lookup(&line, &host, "KEY"), Lookup::Match);
        // A HostName reaches the lookup too.
        let mut options = Options::default();
        options.apply_option("HostName %h.Other.EXAMPLE").unwrap();
        assert_eq!(target_host(&options, &host), "host.example.other.example");
    }

    #[test]
    fn the_escape_filter_acts_only_at_line_start() {
        let run = |input: &[u8]| {
            let mut filter = EscapeFilter::new(DEFAULT_ESCAPE);
            let mut output = Vec::new();
            let disconnect = filter.filter(input, &mut output);
            (output, disconnect)
        };
        assert_eq!(run(b"~."), (Vec::new(), true));
        assert_eq!(run(b"a~."), (b"a~.".to_vec(), false));
        assert_eq!(run(b"a\r~."), (b"a\r".to_vec(), true));
        assert_eq!(run(b"~~."), (b"~.".to_vec(), false));
        assert_eq!(run(b"~x"), (b"~x".to_vec(), false));
        // Split across reads, the state carries.
        let mut filter = EscapeFilter::new(DEFAULT_ESCAPE);
        let mut output = Vec::new();
        assert!(!filter.filter(b"~", &mut output));
        assert!(filter.filter(b".", &mut output));
    }

    #[test]
    fn the_escape_filter_uses_the_configured_character() {
        let run = |escape: u8, input: &[u8]| {
            let mut filter = EscapeFilter::new(escape);
            let mut output = Vec::new();
            let disconnect = filter.filter(input, &mut output);
            (output, disconnect)
        };
        assert_eq!(run(b'%', b"%."), (Vec::new(), true));
        assert_eq!(run(b'%', b"~."), (b"~.".to_vec(), false));
        assert_eq!(run(b'%', b"%%x"), (b"%x".to_vec(), false));
        assert_eq!(run(1, b"\x01."), (Vec::new(), true));
    }

    #[test]
    fn an_escape_exits_255_and_a_closed_channel_exits_as_the_command_did() {
        assert_eq!(session_status(Ending::Escaped, None), EXIT_FAILURE);
        assert_eq!(session_status(Ending::Escaped, Some(0)), EXIT_FAILURE);
        assert_eq!(session_status(Ending::ChannelClosed, Some(0)), 0);
        assert_eq!(session_status(Ending::ChannelClosed, Some(42)), 42);
        assert_eq!(session_status(Ending::ChannelClosed, None), EXIT_FAILURE);
    }

    #[test]
    fn a_pty_needs_a_terminal_or_force() {
        let choice = |args: &[&str], stdin_is_tty: bool| {
            let invocation = parsed(args);
            decide_tty(&invocation, &invocation.options, stdin_is_tty)
        };
        let granted = || TtyChoice {
            request: true,
            no_terminal: false,
        };
        let none = || TtyChoice {
            request: false,
            no_terminal: false,
        };
        let spoken = || TtyChoice {
            request: false,
            no_terminal: true,
        };
        assert_eq!(choice(&["-t", "h"], true), granted());
        assert_eq!(choice(&["-t", "h"], false), spoken());
        assert_eq!(choice(&["-t", "h", "cmd"], false), spoken());
        assert_eq!(choice(&["-tt", "h"], false), granted());
        assert_eq!(choice(&["-T", "-t", "h"], true), none());
        assert_eq!(choice(&["h"], true), granted());
        assert_eq!(choice(&["h"], false), none());
        assert_eq!(choice(&["h", "cmd"], true), none());
    }

    #[test]
    fn send_env_patterns_glob_against_valid_names() {
        let environment = [
            ("LC_ALL", "C"),
            ("LC_TIME", "x"),
            ("LCX", "no"),
            ("LC_BAD NAME", "no"),
            ("LC_=", "no"),
            ("GIT_PROTOCOL", "version=2"),
        ]
        .into_iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)));
        let patterns = ["LC_*".to_string(), "GIT_PROTOCOL".to_string()];
        let found = selected_env(&patterns, environment);
        let names: Vec<&str> = found.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["LC_ALL", "LC_TIME", "GIT_PROTOCOL"]);
        assert_eq!(found[2].1, b"version=2");
        // A value need not be text.
        let raw = vec![(OsString::from("A"), OsString::from_vec(vec![0xff]))];
        assert_eq!(
            selected_env(&["A".to_string()], raw.into_iter()),
            [("A".to_string(), vec![0xff])]
        );
    }

    #[test]
    fn signed_data_is_rfc4252_shaped() {
        let data = signed_auth_data(&[1; 32], "u", b"KEY");
        let mut reader = Reader::new(&data);
        assert_eq!(reader.string().unwrap(), [1u8; 32]);
        assert_eq!(reader.u8().unwrap(), msg::USERAUTH_REQUEST);
        assert_eq!(reader.str().unwrap(), "u");
        assert_eq!(reader.str().unwrap(), "ssh-connection");
        assert_eq!(reader.str().unwrap(), "publickey");
        assert!(reader.bool().unwrap());
        assert_eq!(reader.str().unwrap(), "ssh-ed25519");
        assert_eq!(reader.string().unwrap(), b"KEY");
        assert!(reader.finish().is_ok());
    }

    fn check_for(strict: Strict, user_files: Vec<PathBuf>) -> HostCheck {
        HostCheck {
            name: "h".into(),
            display_host: "h".into(),
            address: "127.0.0.1".into(),
            user_files,
            global_files: Vec::new(),
            strict,
            hash: false,
            batch: true,
            logger: quiet(),
            accepted: None,
            clock: SetupClock::default(),
        }
    }

    #[test]
    fn unknown_and_changed_host_keys_fail_with_the_matched_text() {
        let dir = td_fs::TempDir::new("td-ssh-known").unwrap();
        let file = dir.path().join("known_hosts");
        let blob = kex::ed25519_public_blob(&[1; 32]);
        let other = kex::ed25519_public_blob(&[2; 32]);
        let mut strict = check_for(Strict::Yes, vec![file.clone()]);
        assert!(strict
            .check(&blob)
            .err()
            .unwrap()
            .to_string()
            .contains("Host key verification failed."));
        // BatchMode cannot answer the question.
        let mut ask = check_for(Strict::Ask, vec![file.clone()]);
        assert!(ask.check(&blob).is_err());
        // accept-new records, then the same key passes and another fails.
        let mut accept = check_for(Strict::AcceptNew, vec![file.clone()]);
        accept.check(&blob).unwrap();
        accept.record_accepted();
        strict.check(&blob).unwrap();
        for mode in [Strict::Yes, Strict::AcceptNew, Strict::No, Strict::Ask] {
            let mut check = check_for(mode, vec![file.clone()]);
            assert!(check.check(&other).is_err(), "{mode:?}");
        }
    }

    #[test]
    fn a_rekey_must_present_the_same_host_key() {
        let mut env = client_env();
        let blob = kex::ed25519_public_blob(&[1; 32]);
        env.check_host_key(&blob).unwrap();
        env.check_host_key(&blob).unwrap();
        let other = kex::ed25519_public_blob(&[2; 32]);
        assert!(env.check_host_key(&other).is_err());
    }

    #[test]
    fn a_host_key_of_small_order_is_refused_in_every_mode_and_never_recorded() {
        let dir = td_fs::TempDir::new("td-ssh-known").unwrap();
        let file = dir.path().join("known_hosts");
        let mut identity = [0u8; 32];
        identity[0] = 1;
        let blob = kex::ed25519_public_blob(&identity);
        // A pin on it pins nothing: the same blob is refused all the same.
        fs::write(&file, format!("h ssh-ed25519 {}\n", base64(&blob))).unwrap();
        for mode in [Strict::Yes, Strict::AcceptNew, Strict::No, Strict::Ask] {
            let mut check = check_for(mode, vec![file.clone()]);
            let error = check.check(&blob).err().unwrap();
            assert!(matches!(error, Error::Protocol(_)), "{mode:?}: {error}");
            assert!(check.accepted.is_none(), "{mode:?}");
        }
    }

    #[test]
    fn an_unreadable_known_hosts_file_fails_the_connection() {
        let dir = td_fs::TempDir::new("td-ssh-known").unwrap();
        // A directory reads as an error on any account, root's included.
        let mut check = check_for(Strict::No, vec![dir.path().to_path_buf()]);
        let blob = kex::ed25519_public_blob(&[1; 32]);
        let error = check.check(&blob).err().unwrap().to_string();
        assert!(error.contains("Cannot read known hosts file"), "{error}");
        assert!(check.accepted.is_none());
        // Absent is empty, and an absent global file does not change that.
        let absent = dir.path().join("absent");
        let mut check = check_for(Strict::No, vec![absent.clone()]);
        check.global_files = vec![dir.path().join("absent2")];
        check.check(&blob).unwrap();
        check.record_accepted();
        assert!(fs::read_to_string(&absent).unwrap().contains("ssh-ed25519"));
    }

    #[test]
    fn a_pin_survives_a_line_that_is_not_utf8() {
        let dir = td_fs::TempDir::new("td-ssh-known").unwrap();
        let file = dir.path().join("known_hosts");
        let blob = kex::ed25519_public_blob(&[1; 32]);
        let other = kex::ed25519_public_blob(&[2; 32]);
        let mut bytes = b"\xff\xfe\n".to_vec();
        bytes.extend_from_slice(format!("h ssh-ed25519 {}\n", base64(&blob)).as_bytes());
        fs::write(&file, bytes).unwrap();
        let mut check = check_for(Strict::No, vec![file.clone()]);
        check.check(&blob).unwrap();
        assert!(check.check(&other).is_err());
    }

    #[test]
    fn none_means_no_file_and_dev_null_stays_a_file() {
        let default = [Path::new("/default")];
        let files = |configured: Option<Vec<String>>| {
            known_hosts_files(configured.as_ref(), &default, |value| {
                Ok(PathBuf::from(value))
            })
            .unwrap()
        };
        assert!(files(Some(vec!["none".to_string()])).is_empty());
        assert!(files(Some(vec!["NONE".to_string()])).is_empty());
        assert_eq!(
            files(Some(vec!["/dev/null".to_string()])),
            [PathBuf::from("/dev/null")]
        );
        assert_eq!(files(None), [PathBuf::from("/default")]);

        // With no file, a new host is accepted and written nowhere.
        let blob = kex::ed25519_public_blob(&[1; 32]);
        let mut check = check_for(Strict::No, Vec::new());
        check.check(&blob).unwrap();
        check.record_accepted();
        assert!(check.accepted.is_none());
        // /dev/null keeps reading empty and swallowing what is recorded.
        let mut check = check_for(Strict::No, vec![PathBuf::from("/dev/null")]);
        check.check(&blob).unwrap();
        check.record_accepted();
    }

    #[test]
    fn a_new_entry_starts_its_own_line() {
        let dir = td_fs::TempDir::new("td-ssh-known").unwrap();
        let file = dir.path().join("known_hosts");
        fs::write(&file, "other ssh-ed25519 KEY").unwrap();
        append_known_host(&file, "h ssh-ed25519 NEW\n").unwrap();
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "other ssh-ed25519 KEY\nh ssh-ed25519 NEW\n"
        );
        append_known_host(&file, "i ssh-ed25519 NEWER\n").unwrap();
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "other ssh-ed25519 KEY\nh ssh-ed25519 NEW\ni ssh-ed25519 NEWER\n"
        );
    }

    #[test]
    fn pins_only_in_other_key_types_warn_and_the_host_counts_as_unknown() {
        let dir = td_fs::TempDir::new("td-ssh-known").unwrap();
        let file = dir.path().join("known_hosts");
        let rsa = base64(b"rsa blob");
        fs::write(&file, format!("h ssh-rsa {rsa}\n")).unwrap();
        let blob = kex::ed25519_public_blob(&[1; 32]);
        let mut check = check_for(Strict::Yes, vec![file.clone()]);
        let (verdict, warnings) = check.read_files(&base64(&blob)).unwrap();
        assert_eq!(verdict, Verdict::Unknown);
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0],
            format!(
                "WARNING: ssh-rsa key found for host h\nin {}:1\nssh-rsa key fingerprint {}.",
                file.display(),
                fingerprint_of_blob(b"rsa blob")
            )
        );
        // Strict refuses; accept-new takes the Ed25519 key beside the other pin.
        assert!(check.check(&blob).is_err());
        let mut accept = check_for(Strict::AcceptNew, vec![file.clone()]);
        accept.check(&blob).unwrap();
        accept.record_accepted();
        check.check(&blob).unwrap();
    }

    #[test]
    fn the_host_key_question_is_bounded_for_every_source() {
        let mut asked = 0;
        let result = confirm_host_key(
            || {
                asked += 1;
                Ok("nonsense".to_string())
            },
            "SHA256:fp",
            &quiet(),
        );
        assert!(result.is_err());
        assert_eq!(asked, HOST_KEY_TRIES);
        // A prompt that cannot answer, as at a hung-up terminal, stops at once.
        let mut asked = 0;
        let result = confirm_host_key(
            || {
                asked += 1;
                Err(Error::refused("read_passphrase: end of input"))
            },
            "SHA256:fp",
            &quiet(),
        );
        assert!(result.is_err());
        assert_eq!(asked, 1);
        for answer in ["yes", "SHA256:fp"] {
            confirm_host_key(|| Ok(answer.to_string()), "SHA256:fp", &quiet()).unwrap();
        }
        assert!(confirm_host_key(|| Ok("no".to_string()), "SHA256:fp", &quiet()).is_err());
    }

    #[test]
    fn identities_must_be_the_callers_and_private() {
        let path = Path::new("/k");
        assert_eq!(identity_problem(1000, 0o600, 1000, path), None);
        assert_eq!(identity_problem(1000, 0o400, 1000, path), None);
        let too_open = identity_problem(1000, 0o640, 1000, path).unwrap();
        assert!(too_open.contains("too open"), "{too_open}");
        let not_mine = identity_problem(1001, 0o600, 1000, path).unwrap();
        assert!(not_mine.contains("Bad owner"), "{not_mine}");
        // Root's key is no exception for anyone else.
        assert!(identity_problem(0, 0o600, 1000, path).is_some());
    }

    #[test]
    fn a_key_file_loads_only_when_private() {
        let dir = td_fs::TempDir::new("td-ssh-identity").unwrap();
        let path = dir.path().join("k");
        let text = PrivateKey::from_seed(&[3; 32], "").unwrap().to_file(1);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_identity(&path, &quiet()).is_none());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_identity(&path, &quiet()).is_some());
    }

    #[test]
    fn dash_f_none_reads_nothing_and_the_default_user_file_is_checked() {
        let dir = td_fs::TempDir::new("td-ssh-config").unwrap();
        let home = dir.path();
        let mut options = Options::default();
        read_configuration(&mut options, Some(Path::new("none")), "h", home).unwrap();
        assert!(read_configuration(&mut options, Some(Path::new("absent")), "h", home).is_err());
        let config = dir.path().join(".ssh/config");
        fs::create_dir(dir.path().join(".ssh")).unwrap();
        fs::write(&config, "Port 7\n").unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o666)).unwrap();
        // `-F none` leaves the same file unread.
        read_configuration(&mut options, Some(Path::new("none")), "h", home).unwrap();
        assert_eq!(options.port, None);
        let error = read_configuration(&mut options, None, "h", home)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("Bad owner or permissions on"), "{error}");
        // The same file named by `-F` is the caller's own choice.
        read_configuration(&mut options, Some(&config), "h", home).unwrap();
        assert_eq!(options.port, Some(7));
    }

    #[test]
    fn a_read_limit_covers_setup_and_a_write_limit_covers_keepalive() {
        let options = |args: &[&str]| {
            let mut options = Options::default();
            for arg in args {
                options.apply_option(arg).unwrap();
            }
            options
        };
        assert_eq!(setup_read_limit(&options(&[])), None);
        assert_eq!(
            setup_read_limit(&options(&["ConnectTimeout=7", "ServerAliveInterval=2"])),
            Some(Duration::from_secs(7))
        );
        let alive = options(&["ServerAliveInterval=2", "ServerAliveCountMax=5"]);
        assert_eq!(setup_read_limit(&alive), Some(Duration::from_secs(10)));
        assert_eq!(keepalive_limit(&alive), Some(Duration::from_secs(10)));
        // The default count, and a count of none still bounding one interval.
        let alive = options(&["ServerAliveInterval=2"]);
        assert_eq!(keepalive_limit(&alive), Some(Duration::from_secs(6)));
        let alive = options(&["ServerAliveInterval=2", "ServerAliveCountMax=0"]);
        assert_eq!(keepalive_limit(&alive), Some(Duration::from_secs(2)));
        assert_eq!(keepalive_limit(&options(&["ServerAliveInterval=0"])), None);
        assert_eq!(keepalive_limit(&options(&["ConnectTimeout=3"])), None);
    }

    #[test]
    fn a_stalled_write_fails_as_a_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stream = TcpStream::connect(address).unwrap();
        // Accepted and never read.
        let _peer = listener.accept().unwrap();
        let mut options = Options::default();
        options.apply_option("ServerAliveInterval=1").unwrap();
        options.apply_option("ServerAliveCountMax=1").unwrap();
        apply_socket_limits(&stream, &options).unwrap();
        let mut writer = PacketWriter::new(stream);
        let piece = vec![0u8; 32 * 1024];
        let error = within_limit(move || loop {
            if let Err(error) = writer.write_packet(&piece) {
                break error;
            }
        });
        assert!(is_timeout(&error), "{error}");
        assert_eq!(
            timeout_message("u@h"),
            "Timeout, server u@h not responding."
        );
    }

    /// A shared sink, so a test can read what was written behind a `Conn`.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        /// The unencrypted packets written so far.
        fn messages(&self) -> Vec<Vec<u8>> {
            let bytes = self.0.lock().unwrap().clone();
            let mut reader = PacketReader::new(io::Cursor::new(bytes));
            std::iter::from_fn(|| reader.read_packet().ok()).collect()
        }
    }

    fn client_env() -> ClientEnv {
        ClientEnv {
            random: RandomSource(File::open("/dev/urandom").unwrap()),
            check: check_for(Strict::No, Vec::new()),
            pinned: None,
        }
    }

    fn test_conn() -> (Conn<Shared, ClientEnv>, Shared) {
        let shared = Shared::default();
        // The reader side is never reached: these tests run no key exchange.
        let (rx_keys, receiver) = mpsc::sync_channel(1);
        std::mem::forget(receiver);
        let kex = Kex::new(Role::Client, "SSH-2.0-a", "SSH-2.0-b", DEFAULT_KEX.to_vec());
        let conn = Conn::new(
            PacketWriter::new(shared.clone()),
            rx_keys,
            kex,
            client_env(),
            conn::event_channel().0.flow().clone(),
        );
        (conn, shared)
    }

    fn test_session() -> Session {
        Session {
            peer_channel: None,
            credit: None,
            window: LocalWindow::default(),
            stdout: None,
            stderr: None,
            handles: Vec::new(),
            exit_code: None,
            want_tty: false,
            stdin_is_tty: false,
            guard: None,
            max_packet: CHANNEL_MAX_PACKET,
            start_stdin: None,
            stdin_null: false,
            command: Some(b"cat".to_vec()),
            env: Vec::new(),
            term: "dumb".to_string(),
            size: sys::Winsize::default(),
            pending: VecDeque::new(),
            stdout_broken: false,
            logger: quiet(),
        }
    }

    fn confirmation(peer: u32) -> Vec<u8> {
        let mut message = Writer::message(msg::CHANNEL_OPEN_CONFIRMATION);
        message
            .u32(SESSION_CHANNEL)
            .u32(peer)
            .u32(1 << 20)
            .u32(1 << 15);
        message.into_vec()
    }

    #[test]
    fn stdin_null_sends_eof_right_after_the_session_starts() {
        let (mut conn, shared) = test_conn();
        let mut session = test_session();
        session.stdin_null = true;
        session.on_message(&confirmation(9), 0, &mut conn).unwrap();
        let messages = shared.messages();
        let numbers: Vec<u8> = messages.iter().map(|message| message[0]).collect();
        assert_eq!(numbers, [msg::CHANNEL_REQUEST, msg::CHANNEL_EOF]);
        let mut eof = Reader::new(&messages[1][1..]);
        assert_eq!(eof.u32().unwrap(), 9);

        // Otherwise stdin's own end signals it, and nothing is sent early.
        let (mut conn, shared) = test_conn();
        let mut session = test_session();
        session.on_message(&confirmation(9), 0, &mut conn).unwrap();
        let numbers: Vec<u8> = shared.messages().iter().map(|m| m[0]).collect();
        assert_eq!(numbers, [msg::CHANNEL_REQUEST]);
    }

    #[test]
    fn a_remote_command_and_send_env_values_go_out_as_bytes() {
        let (mut conn, shared) = test_conn();
        let mut session = test_session();
        session.command = Some(b"printf \xff".to_vec());
        session.env = vec![("A".to_string(), vec![0xfe])];
        session.on_message(&confirmation(9), 0, &mut conn).unwrap();
        let messages = shared.messages();
        let mut env = Reader::new(&messages[0][1..]);
        env.u32().unwrap();
        assert_eq!(env.str().unwrap(), "env");
        env.bool().unwrap();
        assert_eq!(env.str().unwrap(), "A");
        assert_eq!(env.string().unwrap(), [0xfe]);
        let mut exec = Reader::new(&messages[1][1..]);
        exec.u32().unwrap();
        assert_eq!(exec.str().unwrap(), "exec");
        exec.bool().unwrap();
        assert_eq!(exec.string().unwrap(), b"printf \xff");
    }

    fn channel_data(number: u8, kind: Option<u32>, data: &[u8]) -> Vec<u8> {
        let mut message = Writer::message(number);
        message.u32(SESSION_CHANNEL);
        if let Some(kind) = kind {
            message.u32(kind);
        }
        message.string(data);
        message.into_vec()
    }

    #[test]
    fn data_with_nowhere_to_go_is_credited_back() {
        let piece = vec![0u8; CHANNEL_MAX_PACKET as usize];
        for (number, kind) in [
            // Data after CHANNEL_EOF, which has dropped both sinks.
            (msg::CHANNEL_DATA, None),
            (msg::CHANNEL_EXTENDED_DATA, Some(msg::EXTENDED_DATA_STDERR)),
            // An extended type td-ssh has no stream for.
            (msg::CHANNEL_EXTENDED_DATA, Some(77)),
        ] {
            let (mut conn, shared) = test_conn();
            let mut session = test_session();
            session.peer_channel = Some(9);
            // Three windows' worth: without credit the first would be the last.
            let total = 3 * CHANNEL_WINDOW as usize;
            for _ in 0..total / piece.len() {
                session
                    .on_message(&channel_data(number, kind, &piece), 0, &mut conn)
                    .unwrap();
            }
            let granted: u64 = shared
                .messages()
                .iter()
                .filter(|message| message[0] == msg::CHANNEL_WINDOW_ADJUST)
                .map(|message| {
                    let mut reader = Reader::new(&message[1..]);
                    assert_eq!(reader.u32().unwrap(), 9);
                    u64::from(reader.u32().unwrap())
                })
                .sum();
            assert!(granted >= 2 * u64::from(CHANNEL_WINDOW), "{granted}");
        }
    }

    #[test]
    fn stderr_data_of_an_unknown_type_never_reaches_stderr() {
        let (mut conn, _shared) = test_conn();
        let mut session = test_session();
        let (sink, drained) = mpsc::channel();
        session.stderr = Some(sink);
        session
            .on_message(
                &channel_data(msg::CHANNEL_EXTENDED_DATA, Some(77), b"x"),
                0,
                &mut conn,
            )
            .unwrap();
        session
            .on_message(
                &channel_data(
                    msg::CHANNEL_EXTENDED_DATA,
                    Some(msg::EXTENDED_DATA_STDERR),
                    b"y",
                ),
                0,
                &mut conn,
            )
            .unwrap();
        assert_eq!(drained.try_recv().unwrap(), b"y");
        assert!(drained.try_recv().is_err());
    }

    #[test]
    fn peer_text_in_a_session_message_is_filtered() {
        let (mut conn, _) = test_conn();
        let mut session = test_session();
        let mut failure = Writer::message(msg::CHANNEL_OPEN_FAILURE);
        failure.u32(0).u32(1).str("no\x1b]0;PWNED\x07").str("");
        let text = session
            .on_message(failure.as_slice(), 0, &mut conn)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(text, "channel open failed: no?]0;PWNED?");
    }

    /// A sink's far end that holds every write until released: a stalled consumer.
    struct Stalled {
        release: mpsc::Receiver<()>,
        seen: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for Stalled {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            let _ = self.release.recv();
            self.seen.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Runs `work` where a hang is a test failure, not a stuck test run.
    fn within_limit<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
        let (done, finished) = mpsc::channel();
        thread::spawn(move || {
            let _ = done.send(work());
        });
        finished.recv_timeout(LIMIT).expect("it hung")
    }

    const QUEUED: usize = 6000;

    fn no_escape() -> EscapeWait<'static> {
        static NEVER: AtomicBool = AtomicBool::new(false);
        EscapeWait {
            escaped: &NEVER,
            patience: Duration::ZERO,
        }
    }

    struct StalledSink {
        events: conn::EventSender,
        receiver: EventReceiver,
        handle: JoinHandle<()>,
        release: mpsc::Sender<()>,
        seen: Arc<Mutex<Vec<u8>>>,
    }

    /// More queued messages than the event queue holds, behind a stalled consumer
    /// that `release` lets go, and the sink's sender already dropped.
    fn stalled_sink() -> StalledSink {
        let (events, receiver) = conn::event_channel();
        let (release, gate) = mpsc::channel();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let writer = Stalled {
            release: gate,
            seen: seen.clone(),
        };
        let (sink, handle) =
            conn::spawn_sink(writer, SESSION_CHANNEL, WhenBroken::Discard, events.clone());
        for _ in 0..QUEUED {
            sink.send(vec![1u8; 100]).unwrap();
        }
        StalledSink {
            events,
            receiver,
            handle: handle.unwrap(),
            release,
            seen,
        }
    }

    #[test]
    fn closing_with_more_output_than_the_queue_holds_finishes_and_loses_nothing() {
        let sink = stalled_sink();
        drop(sink.events);
        for _ in 0..QUEUED {
            sink.release.send(()).unwrap();
        }
        let (handle, receiver) = (sink.handle, sink.receiver);
        within_limit(move || {
            drain_while_joining(vec![handle], &receiver, &no_escape(), &mut |_| {});
        });
        assert_eq!(sink.seen.lock().unwrap().len(), QUEUED * 100);
    }

    #[test]
    fn the_terminal_is_restored_before_a_stalled_consumer_is_waited_for() {
        let sink = stalled_sink();
        drop(sink.events);
        let (handle, receiver, release) = (sink.handle, sink.receiver, sink.release);
        // Only restoring lets the consumer proceed: waiting first would hang.
        within_limit(move || {
            finish_output(
                || {
                    for _ in 0..QUEUED {
                        release.send(()).unwrap();
                    }
                },
                vec![handle],
                &receiver,
                &no_escape(),
                &mut |_| {},
            )
        });
        assert_eq!(sink.seen.lock().unwrap().len(), QUEUED * 100);
    }

    #[test]
    fn a_peer_still_sending_cannot_keep_the_drain_open() {
        let sink = stalled_sink();
        for _ in 0..QUEUED {
            sink.release.send(()).unwrap();
        }
        // The reader thread with a server sending IGNORE for ever.
        let events = sink.events;
        let flood = thread::spawn(move || {
            while events
                .send(Event::Packet {
                    payload: vec![msg::IGNORE],
                    seq: 0,
                })
                .is_ok()
            {}
        });
        let (handle, receiver) = (sink.handle, sink.receiver);
        within_limit(move || {
            drain_while_joining(vec![handle], &receiver, &no_escape(), &mut |_| {});
            // What ends such a reader.
            drop(receiver);
        });
        assert_eq!(sink.seen.lock().unwrap().len(), QUEUED * 100);
        flood.join().unwrap();
    }

    /// The server's half of a key exchange, for tests of what the client does.
    struct ServerEnv {
        signing: ed25519::SigningKey,
    }

    impl ServerEnv {
        fn new() -> Self {
            Self {
                signing: ed25519::SigningKey::from_seed(&[42; 32]).unwrap(),
            }
        }
    }

    impl KexEnvironment for ServerEnv {
        fn random(&mut self, out: &mut [u8]) -> Result<()> {
            File::open("/dev/urandom")?.read_exact(out)?;
            Ok(())
        }
        fn host_key_blob(&mut self) -> Result<Vec<u8>> {
            Ok(kex::ed25519_public_blob(&self.signing.public_key()))
        }
        fn sign(&mut self, hash: &[u8; 32]) -> Result<Vec<u8>> {
            Ok(ed25519_signature_blob(&self.signing.sign(hash).unwrap()))
        }
        fn check_host_key(&mut self, _blob: &[u8]) -> Result<()> {
            Ok(())
        }
    }

    struct FakeServer {
        reader: PacketReader<TcpStream>,
        writer: PacketWriter<TcpStream>,
        kex: Kex,
        env: ServerEnv,
    }

    impl FakeServer {
        fn accept(listener: &TcpListener) -> Self {
            let (mut stream, _) = listener.accept().unwrap();
            let client_version = exchange_versions(&mut stream, false).unwrap();
            let mut env = ServerEnv::new();
            let mut kex = Kex::new(
                Role::Server,
                &client_version,
                IDENTIFICATION,
                DEFAULT_KEX.to_vec(),
            );
            let mut reader = PacketReader::new(stream.try_clone().unwrap());
            let mut writer = PacketWriter::new(stream);
            conn::initial_kex(&mut reader, &mut writer, &mut kex, &mut env).unwrap();
            Self {
                reader,
                writer,
                kex,
                env,
            }
        }

        fn expect(&mut self, number: u8) -> Vec<u8> {
            let payload = self.reader.read_packet().unwrap();
            assert_eq!(payload[0], number);
            payload
        }

        fn send(&mut self, payload: &[u8]) {
            self.writer.write_packet(payload).unwrap();
        }

        /// Starts a rekey, and returns once it finished, with whatever
        /// application messages the client had sent before it saw the KEXINIT.
        fn rekey(&mut self) -> Vec<Vec<u8>> {
            let init = self.kex.start(&mut self.env).unwrap();
            self.send(&init);
            let mut in_flight = Vec::new();
            loop {
                let payload = self.reader.read_packet().unwrap();
                if msg::is_kex(payload[0]) {
                    let finished = conn::kex_step(
                        &mut self.reader,
                        &mut self.writer,
                        &mut self.kex,
                        &mut self.env,
                        &payload,
                    )
                    .unwrap();
                    if finished {
                        return in_flight;
                    }
                } else {
                    in_flight.push(payload);
                }
            }
        }

        /// The client's next request, whether it crossed a rekey or follows it.
        fn request_after_rekey(&mut self) -> Vec<u8> {
            let in_flight = self.rekey();
            let request = in_flight.first().cloned();
            let request = request.unwrap_or_else(|| self.expect(msg::USERAUTH_REQUEST));
            assert_eq!(request[0], msg::USERAUTH_REQUEST);
            request
        }
    }

    /// A consumer that never lets go cannot hold `~.` back.
    #[test]
    fn leaving_by_escape_does_not_wait_for_a_stalled_consumer() {
        let sink = stalled_sink();
        let (handle, receiver, release) = (sink.handle, sink.receiver, sink.release);
        let start = Instant::now();
        finish_output(
            || {},
            vec![handle],
            &receiver,
            &EscapeWait {
                escaped: &AtomicBool::new(true),
                patience: Duration::from_millis(100),
            },
            &mut |_| {},
        );
        assert!(start.elapsed() < LIMIT);
        drop(release);
    }

    /// The flag was once read as the drain began, and `~.` during it did nothing.
    #[test]
    fn an_escape_during_the_drain_ends_it_too() {
        let sink = stalled_sink();
        let (handle, receiver, release) = (sink.handle, sink.receiver, sink.release);
        let escaped = Arc::new(AtomicBool::new(false));
        let pressing = escaped.clone();
        let press = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            pressing.store(true, Ordering::SeqCst);
        });
        let during = escaped.clone();
        let hold = release;
        within_limit(move || {
            let wait = EscapeWait {
                escaped: &during,
                patience: Duration::from_millis(100),
            };
            drain_while_joining(vec![handle], &receiver, &wait, &mut |_| {});
        });
        assert!(escaped.load(Ordering::SeqCst), "ended before the escape");
        press.join().unwrap();
        drop(hold);
    }

    /// Status and end of connection, queued behind a write that then failed.
    #[test]
    fn the_drain_shows_what_was_queued_after_the_sinks_have_gone() {
        let (events, receiver) = conn::event_channel();
        let mut exit = channel_request(0, "exit-status", false);
        exit.u32(7);
        events
            .send(Event::Packet {
                payload: exit.into_vec(),
                seq: 0,
            })
            .unwrap();
        let mut seen = Vec::new();
        drain_while_joining(Vec::new(), &receiver, &no_escape(), &mut |event| {
            if let Event::Packet { payload, .. } = event {
                seen.push(ExitReport::in_packet(&payload));
            }
        });
        assert!(matches!(seen[..], [Some(ExitReport::Status(7))]));
    }

    #[test]
    fn only_a_complete_exit_request_is_a_report() {
        let mut signal = channel_request(0, "exit-signal", false);
        signal.str("TERM").bool(false).str("").str("");
        assert!(matches!(
            ExitReport::in_packet(signal.as_slice()),
            Some(ExitReport::Signal(name)) if name == "TERM"
        ));
        let status = channel_request(0, "exit-status", false);
        assert!(ExitReport::in_packet(status.as_slice()).is_none());
        let other = channel_request(0, "keepalive@openssh.com", true);
        assert!(ExitReport::in_packet(other.as_slice()).is_none());
        assert!(ExitReport::in_packet(&[msg::IGNORE]).is_none());
        assert!(ExitReport::in_packet(&[]).is_none());
    }

    /// A server sending the identification a byte at a time, each inside any limit
    /// on one read, is still held to the whole of `ConnectTimeout`.
    #[test]
    fn connect_timeout_is_one_deadline_however_the_bytes_are_dripped() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            for byte in b"SSH-2.0-a-very-long-version-string-sent-slowly\r\n"
                .iter()
                .cycle()
            {
                if stream.write_all(&[*byte]).is_err() {
                    return;
                }
                if stopped.recv_timeout(Duration::from_millis(100)).is_ok() {
                    return;
                }
            }
        });
        let stream = TcpStream::connect(address).unwrap();
        let mut options = Options::default();
        options.apply_option("ConnectTimeout=1").unwrap();
        apply_socket_limits(&stream, &options).unwrap();
        let start = Instant::now();
        let error = within_limit(move || {
            let identities = test_identity();
            let logger = quiet();
            let login = Login {
                user: "u",
                identities: &identities,
                prompts: 3,
                batch: true,
                logger: &logger,
            };
            let limits = SetupLimits {
                clock: SetupClock::until(Some(Duration::from_secs(1))),
                auth_read: Some(Duration::from_secs(1)),
            };
            establish(stream, client_env(), DEFAULT_KEX.to_vec(), &login, limits)
                .err()
                .unwrap()
        });
        assert!(is_timeout(&error), "{error}");
        assert!(start.elapsed() < Duration::from_secs(4));
        drop(stop);
        server.join().unwrap();
    }

    #[test]
    fn time_at_the_host_key_question_does_not_count_against_the_deadline() {
        let clock = SetupClock::until(Some(Duration::from_secs(1)));
        let before = clock.deadline().unwrap();
        clock.extend(Duration::from_secs(60));
        assert_eq!(clock.deadline().unwrap(), before + Duration::from_secs(60));
        clock.stop();
        assert_eq!(clock.deadline(), None);
        clock.extend(Duration::from_secs(60));
        assert_eq!(clock.deadline(), None);
    }

    /// Past the first exchange a read has a limit of its own, not the deadline.
    #[test]
    fn authentication_reads_under_the_keepalive_bound_else_connect_timeout() {
        let mut options = Options::default();
        assert_eq!(auth_read_limit(&options), None);
        options.apply_option("ConnectTimeout=7").unwrap();
        assert_eq!(auth_read_limit(&options), Some(Duration::from_secs(7)));
        options.apply_option("ServerAliveInterval=2").unwrap();
        options.apply_option("ServerAliveCountMax=3").unwrap();
        assert_eq!(auth_read_limit(&options), keepalive_limit(&options));
        assert!(keepalive_limit(&options).is_some());
    }

    #[test]
    fn every_session_has_a_write_limit_and_keepalive_only_shortens_it() {
        let mut options = Options::default();
        assert_eq!(session_write_limit(&options), SESSION_WRITE_LIMIT);
        options.apply_option("ServerAliveInterval=2").unwrap();
        options.apply_option("ServerAliveCountMax=1").unwrap();
        assert_eq!(session_write_limit(&options), Duration::from_secs(2));
        let mut long = Options::default();
        long.apply_option("ServerAliveInterval=3000").unwrap();
        assert_eq!(session_write_limit(&long), SESSION_WRITE_LIMIT);
    }

    /// Neither of these is fatal or ignored while authenticating (DESIGN.md section 2).
    #[test]
    fn during_auth_unimplemented_is_ignored_and_global_requests_are_answered() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut server = FakeServer::accept(&listener);
            let extras = |server: &mut FakeServer| {
                let mut unimplemented = Writer::message(msg::UNIMPLEMENTED);
                unimplemented.u32(3);
                server.send(&unimplemented.into_vec());
                let mut quiet = Writer::message(msg::GLOBAL_REQUEST);
                quiet.str("nobody-replies").bool(false);
                server.send(&quiet.into_vec());
                let mut asked = Writer::message(msg::GLOBAL_REQUEST);
                asked.str("keepalive@openssh.com").bool(true);
                server.send(&asked.into_vec());
            };
            server.expect(msg::SERVICE_REQUEST);
            // While awaiting the service's acceptance, and while awaiting a reply.
            extras(&mut server);
            let mut accept = Writer::message(msg::SERVICE_ACCEPT);
            accept.str("ssh-userauth");
            server.send(&accept.into_vec());
            assert_eq!(
                server.reader.read_packet().unwrap()[0],
                msg::REQUEST_FAILURE
            );
            server.expect(msg::USERAUTH_REQUEST);
            extras(&mut server);
            assert_eq!(
                server.reader.read_packet().unwrap()[0],
                msg::REQUEST_FAILURE
            );
            server.send(&[msg::USERAUTH_SUCCESS]);
        });
        let stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(LIMIT)).unwrap();
        let identities = test_identity();
        let logger = quiet();
        let login = Login {
            user: "u",
            identities: &identities,
            prompts: 3,
            batch: true,
            logger: &logger,
        };
        let limits = SetupLimits {
            clock: SetupClock::default(),
            auth_read: Some(LIMIT),
        };
        establish(stream, client_env(), DEFAULT_KEX.to_vec(), &login, limits).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn a_connection_lost_after_the_status_returns_the_status() {
        let logger = quiet();
        let lost = |error, status| lost_connection(error, status, &logger, "h").unwrap();
        let eof = || Error::Io(io::ErrorKind::UnexpectedEof.into());
        let reset = || Error::Io(io::ErrorKind::ConnectionReset.into());
        let timeout = || Error::Io(io::ErrorKind::TimedOut.into());
        for error in [eof, reset, timeout] {
            assert_eq!(lost(error(), Some(7)), 7);
        }
        assert_eq!(lost(Error::Disconnected(String::new()), Some(7)), 7);
        assert_eq!(lost(Error::Disconnected("bye".into()), Some(7)), 7);
        // Without one nothing has changed.
        assert_eq!(lost(eof(), None), EXIT_FAILURE);
        assert_eq!(lost(timeout(), None), EXIT_FAILURE);
        assert!(lost_connection(reset(), None, &logger, "h").is_err());
        assert!(lost_connection(Error::protocol("bad"), Some(7), &logger, "h").is_err());
    }

    #[test]
    fn a_long_command_is_refused_by_name_before_anything_connects() {
        check_command_length(&vec![b'x'; MAX_REMOTE_COMMAND]).unwrap();
        let text = check_command_length(&vec![b'x'; 100 * 1024])
            .err()
            .unwrap()
            .to_string();
        assert!(text.contains("remote command is too long"), "{text}");
        assert!(text.contains("102400"), "{text}");
        // What is admitted does go out as one packet.
        let mut request = channel_request(0, "exec", true);
        request.string(&vec![b'x'; MAX_REMOTE_COMMAND]);
        PacketWriter::new(Vec::new())
            .write_packet(request.as_slice())
            .unwrap();
    }

    #[test]
    fn a_host_is_held_to_what_openssh_calls_a_valid_hostname() {
        for good in ["a", "host.example", "[::1]", "::1", "a-b_c", "x%y"] {
            assert_eq!(checked_host(good).unwrap(), good);
        }
        for bad in [
            "",
            "-oProxyCommand=x",
            "a b",
            "a\tb",
            "a\nb",
            "a,b",
            "a*",
            "a?",
            "a|b",
            "a;b",
            "a&b",
            "$a",
            "a`b",
            "a'b",
            "a\"b",
            "a\\b",
            "a<b",
            "a>b",
            "(a)",
            "{a}",
        ] {
            assert!(checked_host(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn identity_falls_back_on_the_real_uids_entry() {
        let table =
            "root:x:0:0:root:/root:/bin/sh\nbad line\nuser:x:1000:100::/home/user:/bin/sh\n";
        assert_eq!(
            passwd_entry(table, 1000),
            Some(("user".to_string(), "/home/user".to_string()))
        );
        assert_eq!(passwd_entry(table, 5), None);
        let entry = || Some(("user".to_string(), "/home/user".to_string()));
        let none = |_: &str| None;
        assert_eq!(
            local_identity(none, entry),
            (PathBuf::from("/home/user"), Some("user".to_string()))
        );
        // No entry at all is no user, not somebody called `unknown`.
        assert_eq!(local_identity(none, || None), (PathBuf::from("/"), None));
        let empty = |_: &str| Some(OsString::new());
        assert_eq!(
            local_identity(empty, entry),
            (PathBuf::from("/home/user"), Some("user".to_string()))
        );
        // The environment still comes first, each variable on its own.
        let logname = |name: &str| (name == "LOGNAME").then(|| OsString::from("other"));
        assert_eq!(
            local_identity(logname, entry),
            (PathBuf::from("/home/user"), Some("other".to_string()))
        );
        let home = |name: &str| (name == "HOME").then(|| OsString::from("/h"));
        assert_eq!(
            local_identity(home, entry),
            (PathBuf::from("/h"), Some("user".to_string()))
        );
        let both = |name: &str| match name {
            "HOME" => Some(OsString::from("/h")),
            "USER" => Some(OsString::from("me")),
            _ => None,
        };
        assert_eq!(
            local_identity(both, || panic!("no need to read the table")),
            (PathBuf::from("/h"), Some("me".to_string()))
        );
    }

    /// Fails with `EAGAIN` before every byte, as a full non-blocking pipe does.
    #[derive(Default)]
    struct Busy {
        data: Vec<u8>,
        busy: bool,
    }

    impl Write for Busy {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.busy = !self.busy;
            if self.busy {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            self.data
                .extend_from_slice(data.get(..1).unwrap_or_default());
            Ok(data.len().min(1))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Read for Busy {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.busy = !self.busy;
            if self.busy {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let Some(next) = self.data.pop() else {
                return Ok(0);
            };
            out[0] = next;
            Ok(1)
        }
    }

    #[test]
    fn an_inherited_nonblocking_stream_loses_nothing() {
        let mut out = Patient(Busy::default());
        out.write_all(b"every byte").unwrap();
        out.flush().unwrap();
        assert_eq!(out.0.data, b"every byte");
        let mut input = Patient(Busy {
            data: b"cba".to_vec(),
            busy: false,
        });
        let mut read = Vec::new();
        input.read_to_end(&mut read).unwrap();
        assert_eq!(read, b"abc");
    }

    #[test]
    fn hashing_that_cannot_get_randomness_records_nothing_in_the_clear() {
        let fails = |_: &mut [u8; 20]| Err(io::ErrorKind::NotFound.into());
        assert!(line_to_record("h", "KEY", true, fails).is_err());
        let line = line_to_record("h", "KEY", true, |salt: &mut [u8; 20]| {
            salt.fill(1);
            Ok(())
        })
        .unwrap();
        assert!(line.starts_with("|1|"), "{line}");
        // Not asked for, nothing is needed from it.
        let line = line_to_record("h", "KEY", false, fails).unwrap();
        assert_eq!(line, "h ssh-ed25519 KEY\n");
    }

    /// Fails closed as a mismatch does, and says what is actually wrong.
    #[test]
    fn a_corrupt_pin_is_named_as_one() {
        let dir = td_fs::TempDir::new("td-ssh-corrupt-pin").unwrap();
        let file = dir.path().join("known_hosts");
        let real = base64(&kex::ed25519_public_blob(&[1; 32]));
        fs::write(&file, "h ssh-ed25519 AAAAbroken\n").unwrap();
        let check = check_for(Strict::No, vec![file.clone()]);
        let (verdict, _) = check.read_files(&real).unwrap();
        assert_eq!(verdict, Verdict::Corrupt(format!("{}:1", file.display())));
        let mut check = check_for(Strict::No, vec![file.clone()]);
        let text = check
            .check(&kex::ed25519_public_blob(&[1; 32]))
            .err()
            .unwrap()
            .to_string();
        assert!(text.contains("verification failed"), "{text}");
        // A changed key among them is the graver reading.
        let other = base64(&kex::ed25519_public_blob(&[2; 32]));
        fs::write(
            &file,
            format!("h ssh-ed25519 AAAAbroken\nh ssh-ed25519 {other}\n"),
        )
        .unwrap();
        let check = check_for(Strict::No, vec![file]);
        assert_eq!(check.read_files(&real).unwrap().0, Verdict::Mismatch);
    }

    /// A refused terminal is told once and stdin still starts, cooked.
    #[test]
    fn a_refused_pty_is_told_and_the_session_goes_on() {
        let (mut conn, wire) = test_conn();
        let mut session = test_session();
        session.want_tty = true;
        let raw = Arc::new(Mutex::new(None));
        let told = raw.clone();
        session.start_stdin = Some(Box::new(move |_, _, went_raw| {
            *told.lock().unwrap() = Some(went_raw.is_some());
        }));
        session.on_message(&confirmation(9), 0, &mut conn).unwrap();
        // Nothing is started before the answer.
        assert_eq!(*raw.lock().unwrap(), None);
        let names: Vec<String> = wire
            .messages()
            .iter()
            .map(|packet| {
                let mut reader = Reader::new(&packet[1..]);
                let _ = reader.u32().unwrap();
                let name = reader.str().unwrap().to_string();
                assert!(reader.bool().unwrap(), "{name} asks for no reply");
                name
            })
            .collect();
        assert_eq!(names, ["pty-req", "exec"]);
        let failure = {
            let mut message = Writer::message(msg::CHANNEL_FAILURE);
            message.u32(SESSION_CHANNEL);
            message.into_vec()
        };
        session.on_message(&failure, 0, &mut conn).unwrap();
        assert_eq!(*raw.lock().unwrap(), Some(false));
        assert!(session.guard.is_none());
        // The next failure is the session's, and that one is fatal.
        assert!(session.on_message(&failure, 0, &mut conn).is_err());
    }

    #[test]
    fn a_granted_pty_starts_stdin_without_raw_when_stdin_is_no_terminal() {
        let (mut conn, _wire) = test_conn();
        let mut session = test_session();
        session.want_tty = true;
        let raw = Arc::new(Mutex::new(None));
        let told = raw.clone();
        session.start_stdin = Some(Box::new(move |_, _, went_raw| {
            *told.lock().unwrap() = Some(went_raw.is_some());
        }));
        session.on_message(&confirmation(9), 0, &mut conn).unwrap();
        let success = {
            let mut message = Writer::message(msg::CHANNEL_SUCCESS);
            message.u32(SESSION_CHANNEL);
            message.into_vec()
        };
        session.on_message(&success, 0, &mut conn).unwrap();
        assert_eq!(*raw.lock().unwrap(), Some(false));
        // The session's own, a success like any other.
        session.on_message(&success, 0, &mut conn).unwrap();
        assert!(session.pending.is_empty());
    }

    #[test]
    fn a_broken_stdout_says_eow_once_and_goes_on_granting_window() {
        let (mut conn, wire) = test_conn();
        let mut session = test_session();
        session.on_message(&confirmation(9), 0, &mut conn).unwrap();
        let before = wire.messages().len();
        session.on_stdout_broken(&mut conn).unwrap();
        let packets = wire.messages();
        let mut reader = Reader::new(&packets[before][1..]);
        assert_eq!(reader.u32().unwrap(), 9);
        assert_eq!(reader.str().unwrap(), "eow@openssh.com");
        assert!(!reader.bool().unwrap());
        let mut data = Writer::message(msg::CHANNEL_DATA);
        data.u32(SESSION_CHANNEL).string(&[0; 1 << 15]);
        // Past the whole window: one shut on dead output would stop stderr with it.
        for _ in 0..100 {
            session.on_message(data.as_slice(), 0, &mut conn).unwrap();
        }
        let packets = wire.messages();
        assert!(packets[before + 1..]
            .iter()
            .all(|packet| packet[0] == msg::CHANNEL_WINDOW_ADJUST));
        assert!(packets.len() > before + 1, "no grant for dead output");
        assert!(session.exit_code.is_none());
    }

    fn test_identity() -> Vec<Identity> {
        let text = PrivateKey::from_seed(&[5; 32], "").unwrap().to_file(1);
        vec![Identity {
            path: PathBuf::from("/test/key"),
            file: PrivateKeyFile::parse(&text).unwrap(),
        }]
    }

    #[test]
    fn a_rekey_during_authentication_completes_the_login() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut server = FakeServer::accept(&listener);
            server.expect(msg::SERVICE_REQUEST);
            let mut accept = Writer::message(msg::SERVICE_ACCEPT);
            accept.str("ssh-userauth");
            server.send(accept.as_slice());
            server.expect(msg::USERAUTH_REQUEST);
            let mut failure = Writer::message(msg::USERAUTH_FAILURE);
            failure.str("publickey").bool(false);
            server.send(failure.as_slice());
            // The client's next message crosses this KEXINIT, as one written
            // after a long passphrase prompt would.
            server.request_after_rekey();
            let mut ok = Writer::message(msg::USERAUTH_PK_OK);
            ok.str(HOST_KEY_ALGORITHM).string(&[0]);
            server.send(ok.as_slice());
            // And again, between the query and the signed request.
            server.request_after_rekey();
            server.send(&[msg::USERAUTH_SUCCESS]);
            server.kex.session_id()
        });
        let stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(LIMIT)).unwrap();
        let identities = test_identity();
        let logger = quiet();
        let login = Login {
            user: "u",
            identities: &identities,
            prompts: 3,
            batch: true,
            logger: &logger,
        };
        let limits = SetupLimits {
            clock: SetupClock::default(),
            auth_read: Some(LIMIT),
        };
        let established =
            establish(stream, client_env(), DEFAULT_KEX.to_vec(), &login, limits).unwrap();
        assert_eq!(server.join().unwrap(), established.kex.session_id());
    }

    #[test]
    fn setup_stalled_at_any_step_times_out() {
        // A server that only exchanges versions, and one that stalls after KEX.
        for stall_after_kex in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let (done, hold) = mpsc::channel::<()>();
            let server = thread::spawn(move || {
                let parked = if stall_after_kex {
                    Err(FakeServer::accept(&listener))
                } else {
                    let (mut stream, _) = listener.accept().unwrap();
                    exchange_versions(&mut stream, false).unwrap();
                    Ok(stream)
                };
                let _ = hold.recv();
                drop(parked);
            });
            let stream = TcpStream::connect(address).unwrap();
            let mut options = Options::default();
            options.apply_option("ConnectTimeout=1").unwrap();
            apply_socket_limits(&stream, &options).unwrap();
            let error = within_limit(move || {
                let identities = test_identity();
                let logger = quiet();
                let login = Login {
                    user: "u",
                    identities: &identities,
                    prompts: 3,
                    batch: true,
                    logger: &logger,
                };
                let limits = SetupLimits {
                    clock: SetupClock::default(),
                    auth_read: Some(Duration::from_secs(1)),
                };
                establish(stream, client_env(), DEFAULT_KEX.to_vec(), &login, limits)
                    .err()
                    .unwrap()
            });
            assert!(is_timeout(&error), "{error}");
            drop(done);
            server.join().unwrap();
        }
    }

    #[test]
    fn a_junk_key_is_not_pinned_and_a_verified_one_is() {
        let dir = td_fs::TempDir::new("td-ssh-record").unwrap();
        let file = dir.path().join("known_hosts");
        let exchange = |tamper: bool| {
            let mut server_env = ServerEnv::new();
            let mut env = client_env();
            env.check = check_for(Strict::AcceptNew, vec![file.clone()]);
            let mut client = Kex::new(Role::Client, "SSH-2.0-a", "SSH-2.0-b", vec![kex::KEX_CURVE]);
            let mut server = Kex::new(Role::Server, "SSH-2.0-a", "SSH-2.0-b", DEFAULT_KEX.to_vec());
            let client_init = client.start(&mut env).unwrap();
            let server_init = server.start(&mut server_env).unwrap();
            let ecdh = client.on_message(&server_init, &mut env).unwrap();
            server.on_message(&client_init, &mut server_env).unwrap();
            let reply = server.on_message(&ecdh.send[0], &mut server_env).unwrap();
            let mut message = reply.send[0].clone();
            if tamper {
                let last = message.len() - 1;
                message[last] ^= 1;
            }
            assert_eq!(client.on_message(&message, &mut env).is_err(), tamper);
        };
        exchange(true);
        assert!(!file.exists(), "a junk key reached known_hosts");
        exchange(false);
        let recorded = fs::read_to_string(&file).unwrap();
        assert!(recorded.starts_with("h ssh-ed25519 "), "{recorded}");
    }

    #[test]
    fn a_host_key_alias_pins_a_nonstandard_port_verbatim() {
        // td-vm-guest's shape: `td-host KEY`, a non-22 Port, strict checking.
        let dir = td_fs::TempDir::new("td-ssh-alias").unwrap();
        let file = dir.path().join("known_hosts");
        let blob = kex::ed25519_public_blob(&[1; 32]);
        fs::write(&file, format!("td-host ssh-ed25519 {}\n", base64(&blob))).unwrap();
        let mut check = check_for(Strict::Yes, vec![file]);
        check.name = known_hosts::host_key_name(Some("td-host"), "10.0.0.5", 2222);
        check.check(&blob).unwrap();
    }
}
