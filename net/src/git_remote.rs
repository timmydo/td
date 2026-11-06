// git-remote-http, git-remote-https — Git's smart-HTTP transport on the rustls
// this crate already links, in place of libcurl and a C TLS library.
//
// Git runs this as a remote helper (gitremote-helpers(7)) with the remote and
// its URL as arguments, and speaks to it over stdin and stdout. It fetches over
// protocol v2 by proxying `stateless-connect`, so Git's own fetch code does the
// negotiating, and pushes by driving `git send-pack --stateless-rpc`.
//
// It is narrower than Git's curl helper and says so instead of pretending:
// no dumb HTTP or WebDAV, no protocol v0 or v1 fetch, no bundle URIs, Basic
// authentication only, and no HTTP/2. A proxy is always tunneled with CONNECT,
// plain http included, so one that allows CONNECT only to 443 refuses a plain
// http remote and a refusal's body goes unseen; a proxy's password goes in its
// URL, and SOCKS is refused. It tries a host's addresses in turn where curl
// races them, so a dead address costs part of the connect budget rather than a
// moment, and it opens a connection per request rather than risk a stale reused
// one, which its HTTP client would not retry.
//
// A setting that asks for something it does not do (client certificates,
// cookies, extra headers, an unverified peer) is refused by name, because
// carrying on without it would be a different request than the one asked for.
// `HARMLESS_CONFIG` lists the only settings it may ignore.
//
// Trust is the CA bundle the image pins, or the one `GIT_SSL_CAINFO` or
// `http.sslCAInfo` names. td-net's compiled-in roots play no part in it.
// Credentials are held for one origin: a redirect elsewhere drops them, a
// redirect from https to http is refused, a password goes only to a server that
// offers Basic, and no message names one. Requests other than the first
// discovery never follow a redirect.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)]
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::pki_types::{pem::PemObject, CertificateDer};
use ureq::config::AutoHeaderValue;
use ureq::{Agent, Proxy, ProxyProtocol, SendBody};

const DEFAULT_CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";
const MAX_CA_BUNDLE: u64 = 16 * 1024 * 1024;
/// A repository with millions of refs advertises hundreds of megabytes.
const MAX_ADVERTISEMENT: u64 = 1024 * 1024 * 1024;
/// A v2 advertisement is capabilities, not refs: this is far past any real one.
const MAX_V2_ADVERTISEMENT: usize = 1024 * 1024;
const MAX_ERROR_BODY: u64 = 64 * 1024;
/// A request this long is sent chunked: Git's `http.postBuffer` default.
const POST_BUFFER: usize = 1024 * 1024;
const MAX_PACKET: usize = 65520;
const MAX_REDIRECTS: u32 = 5;
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);
/// Shared geometrically among a host's addresses, TLS handshake included.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a connection may send nothing. A server preparing a pack sends
/// keepalives, so this is a dead peer, not a slow one.
const STALL_TIMEOUT: Duration = Duration::from_secs(300);

/// Environment that asks for something this transport does not do.
const REFUSED_ENVIRONMENT: &[&str] = &[
    "GIT_SSL_NO_VERIFY",
    "GIT_SSL_CAPATH",
    "GIT_SSL_CERT",
    "GIT_SSL_KEY",
    "GIT_SSL_CERT_TYPE",
    "GIT_SSL_KEY_TYPE",
    "GIT_SSL_CERT_PASSWORD_PROTECTED",
    "GIT_SSL_VERSION",
    "GIT_SSL_CIPHER_LIST",
    "GIT_SSL_PINNEDPUBKEY",
    "GIT_SSL_BACKEND",
    "GIT_HTTP_PROXY_AUTHMETHOD",
    "GIT_PROXY_SSL_CERT",
    "GIT_PROXY_SSL_KEY",
    "GIT_PROXY_SSL_CERT_PASSWORD_PROTECTED",
    "GIT_PROXY_SSL_CAINFO",
];

/// `http.*` settings that change no request: tuning, other platforms' and curl
/// backends', and the server side's.
const HARMLESS_CONFIG: &[&str] = &[
    "http.maxrequests",
    "http.minsessions",
    "http.version",
    "http.maxretries",
    "http.maxretrytime",
    "http.retryafter",
    "http.keepaliveidle",
    "http.keepaliveinterval",
    "http.keepalivecount",
    "http.noepsv",
    "http.sslbackend",
    "http.schannelcheckrevoke",
    "http.schannelusesslcainfo",
    "http.sslautoclientcert",
    "http.getanyfile",
    "http.uploadpack",
    "http.receivepack",
    "http.uploadarchive",
];

pub fn run(args: &[String]) {
    let result = match args {
        [_, remote, url] => session(remote, url),
        // A bundle URI's download names only the URL. It asks for `get`, hears
        // that this has none, and Git goes on without the bundle.
        [_, url] => session(url, url),
        _ => Err("usage: git-remote-https REMOTE URL (git runs this itself)".to_string()),
    };
    if let Err(error) = result {
        // Empty when Git has already been told, as a rejected push's status.
        if !error.is_empty() {
            eprintln!("fatal: {error}");
        }
        std::process::exit(128);
    }
}

fn session(remote: &str, url: &str) -> Result<(), String> {
    let lookup = |name: &str| std::env::var(name).ok();
    refuse_environment(&lookup)?;
    let target = Target::parse(url)?;
    let config = parse_config(&http_config(&target)?);
    let remote_config = parse_config(&remote_proxy_config(remote)?);
    let settings = Settings::new(&target, &config, &remote_config, &lookup)?;
    let mut client = Client::new(target, settings);
    let stdin = io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let mut output = io::stdout();
    let mut options = Options::new();
    loop {
        let Some(line) = read_line(&mut input)? else {
            // Git closed the stream without the blank line: it is gone.
            std::process::exit(1);
        };
        if line.is_empty() {
            return Ok(());
        }
        if line == "capabilities" {
            // No `fetch`: Git fetches through stateless-connect or not at all.
            write_out(
                &mut output,
                b"stateless-connect\noption\npush\nobject-format\n\n",
            )?;
        } else if line == "list" {
            return Err(old_protocol(&client));
        } else if line == "list for-push" {
            let discovery = client.discover("git-receive-pack", options.verbosity)?;
            write_out(
                &mut output,
                discovery.ref_listing(options.object_format).as_bytes(),
            )?;
        } else if let Some(spec) = line.strip_prefix("push ") {
            let mut specs = vec![spec.to_string()];
            loop {
                match read_line(&mut input)? {
                    None => return Ok(()),
                    Some(next) if next.is_empty() => break,
                    Some(next) => match next.strip_prefix("push ") {
                        Some(spec) => specs.push(spec.to_string()),
                        None => {
                            return Err(format!("this transport does not support {next:?}"));
                        }
                    },
                }
            }
            let result = client.push(&specs, &options, &mut output);
            write_out(&mut output, b"\n")?;
            result?;
        } else if let Some(option) = line.strip_prefix("option ") {
            let answer = options.set(option, &mut client.settings);
            write_out(&mut output, answer.as_bytes())?;
        } else if let Some(service) = line.strip_prefix("stateless-connect ") {
            if client.stateless_connect(service, options.verbosity, &mut input, &mut output)? {
                return Ok(());
            }
        } else if line.starts_with("fetch ") {
            return Err(old_protocol(&client));
        } else {
            return Err(format!("unknown command {line:?} from git"));
        }
    }
}

fn old_protocol(client: &Client) -> String {
    format!(
        "'{}' needs Git protocol v2 to fetch over HTTP here: either the server \
         does not speak it or protocol.version asks for an older one",
        client.target.base
    )
}

fn write_out(output: &mut impl Write, bytes: &[u8]) -> Result<(), String> {
    output
        .write_all(bytes)
        .and_then(|()| output.flush())
        .map_err(|e| format!("write to git: {e}"))
}

fn read_line(input: &mut impl BufRead) -> Result<Option<String>, String> {
    let mut bytes = Vec::new();
    let read = input
        .read_until(b'\n', &mut bytes)
        .map_err(|e| format!("read from git: {e}"))?;
    if read == 0 {
        return Ok(None);
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| "git sent a command that is not UTF-8".to_string())
}

fn refuse_environment(lookup: &dyn Fn(&str) -> Option<String>) -> Result<(), String> {
    for name in REFUSED_ENVIRONMENT {
        if lookup(name).is_some_and(|value| !value.is_empty()) {
            return Err(format!(
                "{name} is set, and td's HTTP transport does not support it"
            ));
        }
    }
    if lookup("GIT_SMART_HTTP").is_some_and(|value| is_false(&value)) {
        return Err("GIT_SMART_HTTP turns off the only HTTP transport td has".to_string());
    }
    Ok(())
}

fn is_false(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "false" | "no" | "off" | "0" | ""
    )
}

// ---- the URL ---------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
struct Target {
    /// `scheme://host[:port]/path/`: no credentials, so safe in any message.
    base: String,
    https: bool,
    host: String,
    /// What the URL carried, each percent-decoded.
    user: Option<String>,
    password: Option<String>,
}

impl Target {
    /// The errors here never echo the URL, which may hold a password.
    fn parse(url: &str) -> Result<Self, String> {
        if url.contains('#') {
            return Err("an HTTP remote URL may not hold a fragment".to_string());
        }
        let uri: ureq::http::Uri = url
            .parse()
            .map_err(|_| "the remote URL is not a valid HTTP URL".to_string())?;
        let https = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => return Err("the remote URL is neither http:// nor https://".to_string()),
        };
        if uri.query().is_some() {
            return Err("td's HTTP transport does not take a URL with a query".to_string());
        }
        let authority = uri
            .authority()
            .ok_or_else(|| "the remote URL names no host".to_string())?;
        let (user, password, hostport) = match authority.as_str().rsplit_once('@') {
            None => (None, None, authority.as_str()),
            Some((userinfo, hostport)) => {
                let (user, password) = match userinfo.split_once(':') {
                    Some((user, password)) => (user, Some(percent_decode(password)?)),
                    None => (userinfo, None),
                };
                (Some(percent_decode(user)?), password, hostport)
            }
        };
        let path = uri.path();
        let slash = if path.ends_with('/') { "" } else { "/" };
        let scheme = if https { "https" } else { "http" };
        Ok(Self {
            base: format!("{scheme}://{hostport}{path}{slash}"),
            https,
            host: authority.host().to_string(),
            user,
            password,
        })
    }

    /// Where credentials may go: scheme, host and port.
    fn origin(&self) -> &str {
        let after_scheme = self.base.find("://").map_or(0, |at| at + 3);
        let end = self
            .base
            .get(after_scheme..)
            .and_then(|rest| rest.find('/'))
            .map_or(self.base.len(), |at| at + after_scheme);
        self.base.get(..end).unwrap_or(&self.base)
    }

    /// `origin`, with what writes one host's name two ways made the same.
    fn origin_key(&self) -> String {
        let origin = self.origin().to_ascii_lowercase();
        let default = if self.https { ":443" } else { ":80" };
        match origin.strip_suffix(default) {
            Some(bare) => bare.to_string(),
            None => origin,
        }
    }

    /// The target a redirect of `requested` to `location` moves to, whose
    /// `info/refs` request must end in the `tail` that was asked for.
    fn redirected(&self, requested: &str, location: &str, tail: &str) -> Result<Self, String> {
        let absolute = absolute_location(self, requested, location)?;
        let base = absolute.strip_suffix(tail).ok_or_else(|| {
            format!(
                "'{}' redirected somewhere that is not a Git repository",
                self.base
            )
        })?;
        let mut next = Self::parse(base)?;
        if next.user.is_some() {
            return Err(format!(
                "'{}' redirected to a URL with credentials in it",
                self.base
            ));
        }
        if self.https && !next.https {
            return Err(format!(
                "'{}' redirected from https to plain http, which is refused",
                self.base
            ));
        }
        // What the URL carried stays with its origin, and only there.
        if next.origin_key() == self.origin_key() {
            next.user.clone_from(&self.user);
            next.password.clone_from(&self.password);
        }
        Ok(next)
    }
}

/// A redirect's `Location` as an absolute URL. A relative one holding a dot
/// segment is refused rather than resolved.
fn absolute_location(target: &Target, requested: &str, location: &str) -> Result<String, String> {
    let lower = location.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok(location.to_string());
    }
    if location.contains("://") {
        return Err(format!(
            "'{}' redirected to a scheme this transport does not speak",
            target.base
        ));
    }
    let path = location.split('?').next().unwrap_or(location);
    if path
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return Err(format!(
            "'{}' redirected with a dot segment, which is refused",
            target.base
        ));
    }
    if let Some(rest) = location.strip_prefix("//") {
        let scheme = if target.https { "https" } else { "http" };
        return Ok(format!("{scheme}://{rest}"));
    }
    if location.starts_with('/') {
        return Ok(format!("{}{location}", target.origin()));
    }
    let directory = requested.split('?').next().unwrap_or(requested);
    let directory = directory
        .rfind('/')
        .and_then(|at| directory.get(..=at))
        .unwrap_or(directory);
    Ok(format!("{directory}{location}"))
}

fn percent_decode(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        if byte != b'%' {
            out.push(byte);
            at += 1;
            continue;
        }
        let high = bytes.get(at + 1).and_then(|b| hex_value(*b));
        let low = bytes.get(at + 2).and_then(|b| hex_value(*b));
        let (Some(high), Some(low)) = (high, low) else {
            return Err("the remote URL's credentials hold a bad percent escape".to_string());
        };
        out.push(high << 4 | low);
        at += 3;
    }
    let text = String::from_utf8(out).map_err(|_| "the remote URL's credentials are not UTF-8")?;
    if text.bytes().any(|b| b == b'\n' || b == 0) {
        return Err("the remote URL's credentials hold a newline or NUL".to_string());
    }
    Ok(text)
}

fn hex_value(byte: u8) -> Option<u8> {
    char::from(byte)
        .to_digit(16)
        .and_then(|d| u8::try_from(d).ok())
}

// ---- configuration ---------------------------------------------------------

/// `git config -z` output: `key\nvalue\0`, or `key\0` for a bare key.
fn parse_config(bytes: &[u8]) -> Vec<(String, Option<String>)> {
    bytes
        .split(|&byte| byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let text = String::from_utf8_lossy(entry);
            match text.split_once('\n') {
                Some((key, value)) => (key.to_ascii_lowercase(), Some(value.to_string())),
                None => (text.to_ascii_lowercase(), None),
            }
        })
        .collect()
}

/// Run `git config`, where 1 means nothing matched and anything else that
/// is not success means the settings could not be read: going on without
/// them could mean going on without a proxy or a CA someone configured.
fn git_config(args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .arg("config")
        .args(args)
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("run git config: {e}"))?;
    match output.status.code() {
        Some(0) => Ok(output.stdout),
        Some(1) => Ok(Vec::new()),
        _ => Err(format!("git config failed: {}", output.status)),
    }
}

fn http_config(target: &Target) -> Result<Vec<u8>, String> {
    git_config(&["-z", "--get-urlmatch", "http", &target.base])
}

fn remote_proxy_config(remote: &str) -> Result<Vec<u8>, String> {
    // Git names an unconfigured remote by its URL: nothing can be set for it.
    if remote.contains("://") || remote.contains(['\n', '\0']) {
        return Ok(Vec::new());
    }
    let mut pattern = String::from("^remote\\.");
    for ch in remote.chars() {
        if !ch.is_ascii_alphanumeric() {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push_str("\\.proxy");
    git_config(&["-z", "--get-regexp", &pattern])
}

#[derive(Debug, PartialEq)]
struct Settings {
    ca_bundle: String,
    /// False only for a remote set never to follow one.
    follow_redirects: bool,
    /// `remote.*.proxy` or `http.proxy`, which a redirect does not change.
    configured_proxy: Option<String>,
    /// As given, since a proxy URL may hold a password and is never printed.
    proxy: Option<String>,
    low_speed: Option<LowSpeed>,
    post_buffer: usize,
    user_agent: Option<String>,
    family: ureq::config::IpFamily,
}

impl Settings {
    fn new(
        target: &Target,
        config: &[(String, Option<String>)],
        remote: &[(String, Option<String>)],
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let mut ca_bundle = None;
        let mut follow_redirects = true;
        let mut configured_proxy = None;
        let mut low_speed_limit = 0_u64;
        let mut low_speed_time = 0_u64;
        let mut post_buffer = POST_BUFFER;
        let mut user_agent = None;
        for (key, value) in config {
            let text = value.as_deref();
            match key.as_str() {
                "http.sslcainfo" => {
                    ca_bundle = text
                        .filter(|path| !path.is_empty())
                        .map(|path| expand_path(key, path, lookup))
                        .transpose()?
                }
                "http.sslverify" => {
                    if text.is_some_and(is_false) {
                        return Err(
                            "http.sslVerify=false is refused: td's HTTP transport always \
                             verifies its peer"
                                .to_string(),
                        );
                    }
                }
                "http.followredirects" => {
                    // `true` also lets later requests redirect in Git. Those
                    // never do here; they fail rather than go elsewhere.
                    follow_redirects = text.is_none_or(|value| !is_false(value));
                }
                "http.proxy" => configured_proxy = Some(text.unwrap_or("").to_string()),
                "http.lowspeedlimit" => low_speed_limit = parse_number(key, text)?,
                "http.lowspeedtime" => low_speed_time = parse_number(key, text)?,
                "http.postbuffer" => {
                    // As Git: never smaller than the longest packet it must hold.
                    let bytes = parse_number(key, text)?;
                    post_buffer = usize::try_from(bytes)
                        .unwrap_or(usize::MAX)
                        .max(MAX_PACKET + 4);
                }
                "http.useragent" => user_agent = text.map(str::to_string),
                harmless if HARMLESS_CONFIG.contains(&harmless) => {}
                // An empty value resets what an earlier file set, as a list's
                // does; a refused boolean's empty false asks for nothing either.
                // An empty cookie file is the exception: it turns cookies on.
                other if text == Some("") && other != "http.cookiefile" => {}
                other => {
                    return Err(format!(
                        "{other} is set, and td's HTTP transport does not support it"
                    ))
                }
            }
        }
        for (key, value) in remote {
            if key.ends_with(".proxy") {
                configured_proxy = Some(value.clone().unwrap_or_default());
            } else if key.ends_with(".proxyauthmethod") {
                return Err(format!(
                    "{key} is set, and td's HTTP transport does not support it"
                ));
            }
        }
        if let Some(value) = lookup("GIT_SSL_CAINFO").filter(|value| !value.is_empty()) {
            ca_bundle = Some(value);
        }
        // Empty leaves the setting as it was, as in Git.
        if let Some(value) = lookup("GIT_HTTP_LOW_SPEED_LIMIT").filter(|v| !v.is_empty()) {
            low_speed_limit = parse_number("GIT_HTTP_LOW_SPEED_LIMIT", Some(&value))?;
        }
        if let Some(value) = lookup("GIT_HTTP_LOW_SPEED_TIME").filter(|v| !v.is_empty()) {
            low_speed_time = parse_number("GIT_HTTP_LOW_SPEED_TIME", Some(&value))?;
        }
        if let Some(value) = lookup("GIT_HTTP_USER_AGENT") {
            user_agent = Some(value);
        }
        let low_speed = (low_speed_limit > 0 && low_speed_time > 0).then(|| LowSpeed {
            limit: low_speed_limit,
            time: Duration::from_secs(low_speed_time),
        });
        Ok(Self {
            ca_bundle: ca_bundle.unwrap_or_else(|| DEFAULT_CA_BUNDLE.to_string()),
            follow_redirects,
            proxy: select_proxy(target, configured_proxy.clone(), lookup),
            configured_proxy,
            low_speed,
            post_buffer,
            user_agent,
            family: ureq::config::IpFamily::Any,
        })
    }
}

/// A count as Git's own settings write it, with an optional k, m or g.
fn parse_number(name: &str, value: Option<&str>) -> Result<u64, String> {
    let text = value.unwrap_or("").to_ascii_lowercase();
    let (digits, scale) = match text.strip_suffix(['k', 'm', 'g']) {
        Some(digits) if text.ends_with('k') => (digits, 1 << 10),
        Some(digits) if text.ends_with('m') => (digits, 1 << 20),
        Some(digits) => (digits, 1 << 30),
        None => (text.as_str(), 1),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|count| count.checked_mul(scale))
        .ok_or_else(|| format!("{name} is not a number"))
}

/// A path setting as Git reads it: `~/` is the home directory. The forms this
/// does not expand are refused rather than opened as literal names.
fn expand_path(
    key: &str,
    path: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<String, String> {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = lookup("HOME").ok_or_else(|| format!("{key} names ~ and HOME is not set"))?;
        return Ok(format!("{home}/{rest}"));
    }
    if path.starts_with('~') || path.starts_with(":(") || path.starts_with("%(") {
        return Err(format!(
            "{key} uses a path form td's HTTP transport does not expand"
        ));
    }
    Ok(path.to_string())
}

/// `http.lowSpeedLimit` and `http.lowSpeedTime`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct LowSpeed {
    limit: u64,
    time: Duration,
}

/// A response body held to the low-speed limit: a window of at least `time`
/// holding fewer than `limit * time` bytes ends it. The allowance is the window's
/// nominal one, so upload-pack's five keepalive bytes every five seconds and a
/// little survive a limit of one. A server sending nothing is the per-read
/// timeout's, set to the same `time`; an upload has only the per-write timeout.
struct Paced<R> {
    inner: R,
    low_speed: Option<LowSpeed>,
    since: Instant,
    bytes: u64,
}

impl<R> Paced<R> {
    fn new(inner: R, low_speed: Option<LowSpeed>) -> Self {
        Self {
            inner,
            low_speed,
            since: Instant::now(),
            bytes: 0,
        }
    }

    fn note(&mut self, read: usize, now: Instant) -> io::Result<()> {
        let Some(low_speed) = self.low_speed else {
            return Ok(());
        };
        self.bytes = self.bytes.saturating_add(read as u64);
        let elapsed = now.saturating_duration_since(self.since);
        if elapsed < low_speed.time {
            return Ok(());
        }
        if self.bytes < low_speed.limit.saturating_mul(low_speed.time.as_secs()) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "the transfer stayed below {} bytes a second for {} seconds",
                    low_speed.limit,
                    low_speed.time.as_secs()
                ),
            ));
        }
        self.since = now;
        self.bytes = 0;
        Ok(())
    }
}

impl<R: Read> Read for Paced<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        if read > 0 {
            self.note(read, Instant::now())?;
        }
        Ok(read)
    }
}

/// As Git and curl choose: `remote.*.proxy` or `http.proxy`, where an empty
/// value means none, then the scheme's variable and `all_proxy`. Plain HTTP
/// reads only the lower-case `http_proxy`, which a CGI request cannot set.
/// `no_proxy` wins over all of them.
fn select_proxy(
    target: &Target,
    configured: Option<String>,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let names: &[&str] = if target.https {
        &["https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"]
    } else {
        &["http_proxy", "all_proxy", "ALL_PROXY"]
    };
    let chosen = match configured {
        Some(value) => value,
        None => names
            .iter()
            .find_map(|name| lookup(name).filter(|value| !value.is_empty()))?,
    };
    if chosen.is_empty() {
        return None;
    }
    let bypass = lookup("no_proxy").or_else(|| lookup("NO_PROXY"));
    if bypass.is_some_and(|list| no_proxy_matches(&list, &target.host)) {
        return None;
    }
    Some(chosen)
}

/// A host matches an entry it equals or is a subdomain of; `*` matches all.
fn no_proxy_matches(list: &str, host: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    list.split(',').any(|entry| {
        let entry = entry
            .trim()
            .trim_start_matches('.')
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        !entry.is_empty()
            && (entry == "*"
                || host == entry
                || host
                    .strip_suffix(entry.as_str())
                    .is_some_and(|prefix| prefix.ends_with('.')))
    })
}

fn load_tls(bundle: &str, required: bool) -> Result<ureq::tls::TlsConfig, String> {
    let bytes = match read_bundle(bundle) {
        Ok(bytes) => bytes,
        // A plain http remote has no use for the store. A redirect to https is
        // another origin, so it comes back here as required.
        Err(_) if !required => {
            return Ok(crate::fetchd::tls(
                ureq::tls::RootCerts::new_with_certs(&[]),
            ))
        }
        Err(error) => return Err(error),
    };
    let mut roots = Vec::new();
    let mut check = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(&bytes) {
        let cert = cert.map_err(|e| format!("CA bundle {bundle}: {e}"))?;
        // One root rustls cannot parse is one root not trusted; the rest of a
        // pinned store stands, and an all-unusable one fails below.
        let der = ureq::tls::Certificate::from_der(cert.as_ref()).to_owned();
        if check.add(cert).is_ok() {
            roots.push(der);
        }
    }
    if roots.is_empty() {
        return Err(format!("CA bundle {bundle} holds no usable certificate"));
    }
    Ok(crate::fetchd::tls(ureq::tls::RootCerts::new_with_certs(
        &roots,
    )))
}

fn read_bundle(path: &str) -> Result<Vec<u8>, String> {
    // /etc/ssl's bundle is a symlink into the store, so a link is expected.
    let file = std::fs::File::open(path).map_err(|e| format!("CA bundle {path}: {e}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_CA_BUNDLE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("CA bundle {path}: {e}"))?;
    if bytes.len() as u64 > MAX_CA_BUNDLE {
        return Err(format!("CA bundle {path} is too large"));
    }
    Ok(bytes)
}

// ---- options ---------------------------------------------------------------

struct Options {
    verbosity: i32,
    progress: bool,
    dry_run: bool,
    atomic: bool,
    object_format: bool,
    force_if_includes: bool,
    signed: Option<&'static str>,
    push_options: Vec<String>,
    leases: Vec<String>,
}

impl Options {
    fn new() -> Self {
        Self {
            verbosity: 1,
            progress: io::stderr().is_terminal(),
            dry_run: false,
            atomic: false,
            object_format: false,
            force_if_includes: false,
            signed: None,
            push_options: Vec::new(),
            leases: Vec::new(),
        }
    }

    /// Answer one `option NAME [VALUE]` line.
    fn set(&mut self, option: &str, settings: &mut Settings) -> &'static str {
        const OK: &str = "ok\n";
        const INVALID: &str = "error invalid value\n";
        let (name, value) = option.split_once(' ').unwrap_or((option, "true"));
        let boolean = match value {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        };
        let flag = match name {
            "progress" => Some(&mut self.progress),
            "dry-run" => Some(&mut self.dry_run),
            "atomic" => Some(&mut self.atomic),
            "force-if-includes" => Some(&mut self.force_if_includes),
            _ => None,
        };
        if let Some(flag) = flag {
            return match boolean {
                Some(on) => {
                    *flag = on;
                    OK
                }
                None => INVALID,
            };
        }
        match name {
            "verbosity" => match value.parse() {
                Ok(level) => self.verbosity = level,
                Err(_) => return INVALID,
            },
            "object-format" => {
                if boolean != Some(true) {
                    return INVALID;
                }
                self.object_format = true;
            }
            "pushcert" => {
                self.signed = match value {
                    "true" => Some("--signed=yes"),
                    "false" => None,
                    "if-asked" => Some("--signed=if-asked"),
                    _ => return INVALID,
                }
            }
            "cas" => match unquote(value) {
                Some(lease) => self.leases.push(format!("--force-with-lease={lease}")),
                None => return INVALID,
            },
            "push-option" => match unquote(value) {
                Some(option) => self.push_options.push(option),
                None => return INVALID,
            },
            "family" => {
                settings.family = match value {
                    "ipv4" => ureq::config::IpFamily::Ipv4Only,
                    "ipv6" => ureq::config::IpFamily::Ipv6Only,
                    "all" => ureq::config::IpFamily::Any,
                    _ => return INVALID,
                }
            }
            // These shape a fetch, and Git's own code does the fetching over
            // stateless-connect, where it applies them itself.
            "depth" | "deepen-since" | "deepen-not" | "deepen-relative" | "followtags"
            | "cloning" | "update-shallow" | "from-promisor" | "refetch" | "filter" => {}
            _ => return "unsupported\n",
        }
        OK
    }
}

/// Undo Git's `quote_c_style`; a value without a leading quote is literal.
fn unquote(value: &str) -> Option<String> {
    let Some(rest) = value.strip_prefix('"') else {
        return Some(value.to_string());
    };
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    loop {
        let byte = *bytes.get(at)?;
        at += 1;
        match byte {
            b'"' => break,
            b'\\' => {
                let escape = *bytes.get(at)?;
                at += 1;
                match escape {
                    b'a' => out.push(7),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'v' => out.push(11),
                    b'\\' | b'"' => out.push(escape),
                    b'0'..=b'3' => {
                        let middle = bytes.get(at).and_then(|b| octal(*b))?;
                        let low = bytes.get(at + 1).and_then(|b| octal(*b))?;
                        at += 2;
                        out.push((escape - b'0') << 6 | middle << 3 | low);
                    }
                    _ => return None,
                }
            }
            other => out.push(other),
        }
    }
    if at != bytes.len() {
        return None;
    }
    String::from_utf8(out).ok()
}

fn octal(byte: u8) -> Option<u8> {
    matches!(byte, b'0'..=b'7').then(|| byte - b'0')
}

// ---- pkt-lines -------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Packet<'a> {
    Flush,
    Delim,
    ResponseEnd,
    Data(&'a [u8]),
}

fn packet_length(header: &[u8]) -> Option<usize> {
    let mut length = 0_usize;
    if header.len() != 4 {
        return None;
    }
    for byte in header {
        length = length * 16 + usize::from(hex_value(*byte)?);
    }
    Some(length)
}

fn next_packet<'a>(bytes: &'a [u8], at: &mut usize) -> Result<Packet<'a>, String> {
    let bad = || "invalid server response: bad packet framing".to_string();
    let end = at.checked_add(4).ok_or_else(bad)?;
    let length = bytes
        .get(*at..end)
        .and_then(packet_length)
        .ok_or_else(bad)?;
    let (packet, next) = match length {
        0 => (Packet::Flush, end),
        1 => (Packet::Delim, end),
        2 => (Packet::ResponseEnd, end),
        3 => return Err(bad()),
        _ if length > MAX_PACKET + 4 => return Err(bad()),
        _ => {
            let next = at.checked_add(length).ok_or_else(bad)?;
            (Packet::Data(bytes.get(end..next).ok_or_else(bad)?), next)
        }
    };
    *at = next;
    Ok(packet)
}

fn chomp(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\n").unwrap_or(line)
}

fn server_error(line: &[u8]) -> Option<String> {
    line.strip_prefix(b"ERR ")
        .map(|message| format!("remote error: {}", printable(chomp(message))))
}

/// A server's text with its control characters made visible.
fn printable(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|ch| {
            if ch.is_control() && ch != '\t' {
                '?'
            } else {
                ch
            }
        })
        .collect()
}

#[derive(Debug)]
struct Discovery {
    service: String,
    v2: bool,
    /// The advertisement past the service header, as Git's code wants it.
    body: Arc<Vec<u8>>,
    refs: Vec<(String, String)>,
    object_format: String,
}

impl Discovery {
    fn ref_listing(&self, say_format: bool) -> String {
        let mut out = String::new();
        if say_format {
            out.push_str(&format!(":object-format {}\n", self.object_format));
        }
        for (oid, name) in &self.refs {
            out.push_str(&format!("{oid} {name}\n"));
        }
        out.push('\n');
        out
    }
}

fn parse_advertisement(
    service: &str,
    content_type: &str,
    mut body: Vec<u8>,
) -> Result<Discovery, String> {
    if content_type != format!("application/x-{service}-advertisement") {
        return Err(
            "the server does not speak smart HTTP, the only HTTP transport td has".to_string(),
        );
    }
    let mut at = 0;
    let first = match next_packet(&body, &mut at)? {
        Packet::Data(line) => chomp(line),
        _ => return Err("invalid server response: expected a service line".to_string()),
    };
    if let Some(message) = server_error(first) {
        return Err(message);
    }
    let start = if first == format!("# service={service}").as_bytes() {
        while matches!(next_packet(&body, &mut at)?, Packet::Data(_)) {}
        at
    } else if first == b"version 2" {
        0
    } else {
        return Err(format!(
            "invalid server response: got {:?}",
            printable(first)
        ));
    };
    let rest = body.get(start..).unwrap_or(&[]);
    let mut at = 0;
    let v2 = match next_packet(rest, &mut at)? {
        Packet::Data(line) => match server_error(line) {
            Some(message) => return Err(message),
            None => chomp(line) == b"version 2",
        },
        _ => false,
    };
    let (refs, object_format) = if v2 {
        // Git is handed this whole, then waits for its flush: one that never
        // came would leave it and this helper waiting on each other.
        if rest.len() > MAX_V2_ADVERTISEMENT {
            return Err("invalid server response: the advertisement is too long".to_string());
        }
        while next_packet(rest, &mut at)? != Packet::Flush {}
        if at != rest.len() {
            return Err("invalid server response: data past the advertisement".to_string());
        }
        (Vec::new(), "sha1".to_string())
    } else {
        parse_refs(rest)?
    };
    body.drain(..start);
    Ok(Discovery {
        service: service.to_string(),
        v2,
        body: Arc::new(body),
        refs,
        object_format,
    })
}

/// A v0 ref advertisement: the first line carries capabilities after a NUL,
/// and only proper `refs/` names are for a push to see.
fn parse_refs(bytes: &[u8]) -> Result<(Vec<(String, String)>, String), String> {
    let mut refs = Vec::new();
    let mut format = "sha1".to_string();
    let mut at = 0;
    loop {
        let line = match next_packet(bytes, &mut at)? {
            Packet::Flush => break,
            Packet::Data(line) => chomp(line),
            _ => return Err("invalid server response: bad ref advertisement".to_string()),
        };
        if let Some(message) = server_error(line) {
            return Err(message);
        }
        if line == b"version 1" || line.starts_with(b"shallow ") {
            continue;
        }
        let (entry, capabilities) = match line.iter().position(|&byte| byte == 0) {
            Some(nul) => (
                line.get(..nul).unwrap_or(&[]),
                line.get(nul + 1..).unwrap_or(&[]),
            ),
            None => (line, &[][..]),
        };
        for word in capabilities.split(|&byte| byte == b' ') {
            if let Some(name) = word.strip_prefix(b"object-format=") {
                format = match name {
                    b"sha1" => "sha1",
                    b"sha256" => "sha256",
                    _ => return Err("the server uses an unknown object format".to_string()),
                }
                .to_string();
            }
        }
        // Git allows such a name. It cannot be spoken here, so a push sees the
        // repository without that ref rather than failing for all the others.
        let Ok(text) = std::str::from_utf8(entry) else {
            continue;
        };
        let (oid, name) = text
            .split_once(' ')
            .ok_or_else(|| "invalid server response: bad ref line".to_string())?;
        if !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) || !matches!(oid.len(), 40 | 64) {
            return Err("invalid server response: bad object id".to_string());
        }
        if name.starts_with("refs/") && !name.ends_with("^{}") && valid_refname(name) {
            refs.push((oid.to_string(), name.to_string()));
        }
    }
    Ok((refs, format))
}

/// git-check-ref-format's rules, so a server's name cannot forge a helper line.
fn valid_refname(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("@{")
        && name.bytes().all(|byte| {
            byte > 0x20
                && byte != 0x7f
                && !matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
        && name
            .split('/')
            .all(|part| !part.starts_with('.') && !part.ends_with(".lock"))
}

/// What a frame did to the request being assembled.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Frame {
    Flush,
    Other,
    Eof,
}

/// Append one pkt-line from `input` to `out`, with its length header if
/// `framed`: stateless-connect relays Git's framing, and a push strips it.
fn read_frame(input: &mut impl Read, framed: bool, out: &mut Vec<u8>) -> io::Result<Frame> {
    let mut header = [0_u8; 4];
    let mut filled = 0;
    while filled < header.len() {
        let read = input.read(header.get_mut(filled..).unwrap_or(&mut []))?;
        if read == 0 {
            return if filled == 0 {
                Ok(Frame::Eof)
            } else {
                Err(io::ErrorKind::UnexpectedEof.into())
            };
        }
        filled += read;
    }
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "bad packet length from git");
    let length = packet_length(&header).ok_or_else(invalid)?;
    if framed {
        out.extend_from_slice(&header);
    }
    match length {
        0 => Ok(Frame::Flush),
        1 => Ok(Frame::Other),
        2 | 3 => Err(invalid()),
        _ if length > MAX_PACKET + 4 => Err(invalid()),
        _ => {
            let payload = (length - 4) as u64;
            let start = out.len();
            input.by_ref().take(payload).read_to_end(out)?;
            if (out.len() - start) as u64 != payload {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            Ok(Frame::Other)
        }
    }
}

/// A request too long to buffer: what was read, then the rest as it comes.
struct Streamed<'a, R> {
    pending: Vec<u8>,
    sent: usize,
    input: &'a mut R,
    framed: bool,
    ended: bool,
}

impl<R: Read> Read for Streamed<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.sent >= self.pending.len() {
            if self.ended {
                return Ok(0);
            }
            self.pending.clear();
            self.sent = 0;
            match read_frame(self.input, self.framed, &mut self.pending)? {
                Frame::Flush => self.ended = true,
                Frame::Other => {}
                Frame::Eof => return Err(io::ErrorKind::UnexpectedEof.into()),
            }
        }
        let rest = self.pending.get(self.sent..).unwrap_or(&[]);
        let count = rest.len().min(buf.len());
        if let (Some(to), Some(from)) = (buf.get_mut(..count), rest.get(..count)) {
            to.copy_from_slice(from);
        }
        self.sent += count;
        Ok(count)
    }
}

/// Holds a response to pkt-line framing as it passes, like Git's own check, so
/// a truncated or corrupt one is an error and not a confused parser later.
#[derive(Default)]
struct FrameCheck {
    header: [u8; 4],
    filled: usize,
    remaining: usize,
}

impl FrameCheck {
    fn feed(&mut self, mut data: &[u8]) -> Result<(), String> {
        while !data.is_empty() {
            if self.remaining == 0 {
                let take = (self.header.len() - self.filled).min(data.len());
                if let (Some(to), Some(from)) = (
                    self.header.get_mut(self.filled..self.filled + take),
                    data.get(..take),
                ) {
                    to.copy_from_slice(from);
                }
                self.filled += take;
                data = data.get(take..).unwrap_or(&[]);
                if self.filled == self.header.len() {
                    self.filled = 0;
                    let length = packet_length(&self.header)
                        .ok_or_else(|| "invalid server response: bad packet length".to_string())?;
                    self.remaining = match length {
                        0 | 1 => 0,
                        2 => {
                            return Err(
                                "invalid server response: unexpected response end".to_string()
                            )
                        }
                        3 => return Err("invalid server response: bad packet length".to_string()),
                        _ if length > MAX_PACKET + 4 => {
                            return Err("invalid server response: packet too long".to_string())
                        }
                        _ => length - 4,
                    };
                }
            }
            if self.remaining > 0 {
                let take = self.remaining.min(data.len());
                self.remaining -= take;
                data = data.get(take..).unwrap_or(&[]);
            }
        }
        Ok(())
    }

    fn finish(&self) -> Result<(), String> {
        if self.filled != 0 || self.remaining != 0 {
            return Err("invalid server response: the response was cut short".to_string());
        }
        Ok(())
    }
}

// ---- credentials -----------------------------------------------------------

struct Credential {
    user: String,
    password: String,
    /// From a credential helper or prompt, so it may be approved or rejected.
    /// One from the URL is the user's own and stays out of their store.
    filled: bool,
}

impl Credential {
    fn header(&self) -> String {
        let pair = format!("{}:{}", self.user, self.password);
        format!("Basic {}", base64(pair.as_bytes()))
    }
}

pub(crate) fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let byte = |i: usize| chunk.get(i).copied().unwrap_or(0);
        let word = u32::from(byte(0)) << 16 | u32::from(byte(1)) << 8 | u32::from(byte(2));
        for (position, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if position <= chunk.len() {
                let index = (word >> shift & 0x3f) as usize;
                out.push(char::from(ALPHABET.get(index).copied().unwrap_or(b'=')));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// One `git credential` conversation; its stderr and prompt are Git's own.
fn git_credential(action: &str, description: &str) -> Result<Vec<u8>, String> {
    let mut child = Command::new("git")
        .args(["credential", action])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("run git credential: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(description.as_bytes())
            .map_err(|e| format!("write to git credential: {e}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("run git credential: {e}"))?;
    if !output.status.success() {
        return Err(format!("git credential {action} failed"));
    }
    Ok(output.stdout)
}

fn describe_credential(base: &str, user: Option<&str>, password: Option<&str>) -> String {
    let mut text = format!("url={base}\n");
    if let Some(user) = user {
        text.push_str(&format!("username={user}\n"));
    }
    if let Some(password) = password {
        text.push_str(&format!("password={password}\n"));
    }
    text.push('\n');
    text
}

// ---- the client ------------------------------------------------------------

type Response = ureq::http::Response<ureq::Body>;

fn content_type(response: &Response) -> String {
    response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(|value| value.trim().to_ascii_lowercase())
        .unwrap_or_default()
}

fn offers_basic(response: &Response) -> bool {
    response
        .headers()
        .get_all("www-authenticate")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|part| {
            part.trim_start()
                .get(..5)
                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("basic"))
        })
}

/// A failed response's text/plain body, as Git shows it.
fn show_message(response: Response, low_speed: Option<LowSpeed>) {
    if content_type(&response) != "text/plain" {
        return;
    }
    let mut body = Vec::new();
    if Paced::new(response.into_body().into_reader(), low_speed)
        .take(MAX_ERROR_BODY)
        .read_to_end(&mut body)
        .is_err()
    {
        return;
    }
    for line in printable_lines(&body) {
        eprintln!("remote: {line}");
    }
}

fn printable_lines(body: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed
        .lines()
        .map(|line| printable(line.as_bytes()))
        .collect()
}

struct Client {
    target: Target,
    settings: Settings,
    agent: Option<Agent>,
    credential: Option<Credential>,
    approved: bool,
    discovery: Option<Discovery>,
}

impl Client {
    fn new(target: Target, settings: Settings) -> Self {
        Self {
            target,
            settings,
            agent: None,
            credential: None,
            approved: false,
            discovery: None,
        }
    }

    /// Built on first use, after Git has said `option family`.
    fn agent(&mut self) -> Result<Agent, String> {
        if let Some(agent) = &self.agent {
            return Ok(agent.clone());
        }
        let proxy = self
            .settings
            .proxy
            .as_deref()
            .map(usable_proxy)
            .transpose()?;
        let tls = load_tls(&self.settings.ca_bundle, self.target.https)?;
        let user_agent = self.user_agent();
        let stall = self
            .settings
            .low_speed
            .map_or(STALL_TIMEOUT, |low_speed| low_speed.time);
        let agent = Agent::new_with_config(
            Agent::config_builder()
                .proxy(proxy)
                .http_status_as_error(false)
                .max_redirects(0)
                .max_redirects_will_error(false)
                .redirect_auth_headers(ureq::config::RedirectAuthHeaders::Never)
                .accept_encoding(AutoHeaderValue::None)
                .user_agent(user_agent.as_str())
                .tls_config(tls)
                .ip_family(self.settings.family)
                .max_idle_connections(0)
                .max_idle_connections_per_host(0)
                .timeout_resolve(Some(RESOLVE_TIMEOUT))
                .timeout_connect(Some(CONNECT_TIMEOUT))
                .timeout_per_read(Some(stall))
                .timeout_per_write(Some(stall))
                .build(),
        );
        self.agent = Some(agent.clone());
        Ok(agent)
    }

    /// `git/VERSION`, which servers key behavior on, unless one was set.
    fn user_agent(&self) -> String {
        if let Some(agent) = &self.settings.user_agent {
            return agent.clone();
        }
        let version = Command::new("git")
            .arg("version")
            .output()
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|text| text.split_whitespace().nth(2).map(str::to_string));
        match version {
            Some(version) => format!("git/{version}"),
            None => "git/td".to_string(),
        }
    }

    fn unable(&self, why: impl std::fmt::Display) -> String {
        format!("unable to access '{}': {why}", self.target.base)
    }

    fn authorization(&self) -> Option<String> {
        self.credential.as_ref().map(Credential::header)
    }

    /// Answer a 401: a credential already sent has failed, otherwise find one.
    fn reauthenticate(&mut self, basic: bool) -> Result<(), String> {
        let failed = format!("Authentication failed for '{}'", self.target.base);
        if let Some(credential) = self.credential.take() {
            if credential.filled {
                let description = describe_credential(
                    &self.target.base,
                    Some(&credential.user),
                    Some(&credential.password),
                );
                let _ = git_credential("reject", &description);
            }
            return Err(failed);
        }
        if !basic {
            return Err(format!("{failed}: the server does not offer Basic"));
        }
        if let (Some(user), Some(password)) = (&self.target.user, &self.target.password) {
            self.credential = Some(Credential {
                user: user.clone(),
                password: password.clone(),
                filled: false,
            });
            return Ok(());
        }
        let description = describe_credential(&self.target.base, self.target.user.as_deref(), None);
        let answer = git_credential("fill", &description).map_err(|_| failed.clone())?;
        let answer = String::from_utf8_lossy(&answer);
        let field = |name: &str| {
            answer
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .map(str::to_string)
        };
        match (field("username="), field("password=")) {
            (Some(user), Some(password)) => {
                self.credential = Some(Credential {
                    user,
                    password,
                    filled: true,
                });
                Ok(())
            }
            _ => Err(failed),
        }
    }

    /// Tell Git's credential store once that a filled credential worked.
    fn approve(&mut self) {
        if self.approved {
            return;
        }
        if let Some(credential) = self.credential.as_ref().filter(|c| c.filled) {
            self.approved = true;
            let description = describe_credential(
                &self.target.base,
                Some(&credential.user),
                Some(&credential.password),
            );
            let _ = git_credential("approve", &description);
        }
    }

    fn discover(&mut self, service: &str, verbosity: i32) -> Result<&Discovery, String> {
        if self
            .discovery
            .as_ref()
            .is_none_or(|known| known.service != service)
        {
            let found = self.discover_new(service, verbosity)?;
            self.discovery = Some(found);
        }
        self.discovery
            .as_ref()
            .ok_or_else(|| "lost the discovery".to_string())
    }

    fn discover_new(&mut self, service: &str, verbosity: i32) -> Result<Discovery, String> {
        let tail = format!("info/refs?service={service}");
        let mut hops = 0;
        loop {
            let agent = self.agent()?;
            let url = format!("{}{tail}", self.target.base);
            let mut request = agent
                .get(&url)
                .header("Accept", "*/*")
                .header("Pragma", "no-cache");
            if service == "git-upload-pack" {
                request = request.header("Git-Protocol", "version=2");
            }
            if let Some(authorization) = self.authorization() {
                request = request.header("Authorization", &authorization);
            }
            let response = request.call().map_err(|e| self.unable(e))?;
            let status = response.status().as_u16();
            match status {
                200..=299 => {
                    self.approve();
                    let kind = content_type(&response);
                    let mut body = Vec::new();
                    Paced::new(response.into_body().into_reader(), self.settings.low_speed)
                        .take(MAX_ADVERTISEMENT + 1)
                        .read_to_end(&mut body)
                        .map_err(|e| self.unable(e))?;
                    if body.len() as u64 > MAX_ADVERTISEMENT {
                        return Err(self.unable("the ref advertisement is too large"));
                    }
                    return parse_advertisement(service, &kind, body);
                }
                301 | 302 | 303 | 307 | 308 => {
                    hops += 1;
                    if !self.settings.follow_redirects {
                        return Err(self.unable("the server redirected, and redirects are off"));
                    }
                    if hops > MAX_REDIRECTS {
                        return Err(self.unable("too many redirects"));
                    }
                    let location = response
                        .headers()
                        .get("location")
                        .and_then(|value| value.to_str().ok())
                        .ok_or_else(|| self.unable("a redirect with no location"))?
                        .to_string();
                    let next = self.target.redirected(&url, &location, &tail)?;
                    if next.origin_key() != self.target.origin_key() {
                        self.credential = None;
                        self.approved = false;
                        // The proxy and the store are chosen per origin: go on
                        // with the old one's and a no_proxy host could carry a
                        // request past the proxy meant for the new one.
                        let lookup = |name: &str| std::env::var(name).ok();
                        self.settings.proxy =
                            select_proxy(&next, self.settings.configured_proxy.clone(), &lookup);
                        self.agent = None;
                    }
                    if verbosity > 0 {
                        eprintln!("warning: redirecting to {}", next.base);
                    }
                    self.target = next;
                }
                401 => {
                    let basic = offers_basic(&response);
                    let had = self.credential.is_some();
                    let outcome = self.reauthenticate(basic);
                    if outcome.is_err() || had {
                        show_message(response, self.settings.low_speed);
                    }
                    outcome?;
                }
                404 => {
                    show_message(response, self.settings.low_speed);
                    return Err(format!("repository '{}' not found", self.target.base));
                }
                429 => {
                    show_message(response, self.settings.low_speed);
                    return Err(format!(
                        "rate limited by '{}', please try again later",
                        self.target.base
                    ));
                }
                _ => {
                    show_message(response, self.settings.low_speed);
                    return Err(self.unable(format!("The requested URL returned error: {status}")));
                }
            }
        }
    }

    /// Returns whether a stateless connection ran; if not, it said `fallback`.
    fn stateless_connect(
        &mut self,
        service: &str,
        verbosity: i32,
        input: &mut impl Read,
        output: &mut impl Write,
    ) -> Result<bool, String> {
        if !matches!(service, "git-upload-pack" | "git-upload-archive") {
            write_out(output, b"fallback\n")?;
            return Ok(false);
        }
        let advertisement = {
            let discovery = self.discover("git-upload-pack", verbosity)?;
            discovery.v2.then(|| Arc::clone(&discovery.body))
        };
        let Some(advertisement) = advertisement else {
            write_out(output, b"fallback\n")?;
            return Ok(false);
        };
        write_out(output, b"\n")?;
        // upload-archive has no capability listing of its own to replay.
        if service == "git-upload-pack" {
            write_out(output, advertisement.as_slice())?;
        }
        loop {
            let mut request = Vec::new();
            let first =
                read_frame(input, true, &mut request).map_err(|e| format!("read from git: {e}"))?;
            if first == Frame::Eof {
                return Ok(true);
            }
            let ended = first == Frame::Flush;
            let mut check = FrameCheck::default();
            let result = self.exchange(service, request, ended, input, true, output, &mut check);
            let result = result.and_then(|()| check.finish());
            // Git's reader is waiting for this end however the request went.
            let ended = write_out(output, b"0002");
            result?;
            ended?;
        }
    }

    /// Send one request, assembled from `input` until its flush, and relay the
    /// response. The first read has already gone into `request`.
    #[allow(clippy::too_many_arguments)]
    fn exchange(
        &mut self,
        service: &str,
        mut request: Vec<u8>,
        mut ended: bool,
        input: &mut impl Read,
        framed: bool,
        output: &mut impl Write,
        check: &mut FrameCheck,
    ) -> Result<(), String> {
        let read_error = |e: io::Error| format!("read the request from git: {e}");
        while !ended && request.len() < self.settings.post_buffer {
            match read_frame(input, framed, &mut request).map_err(read_error)? {
                Frame::Flush => ended = true,
                Frame::Other => {}
                Frame::Eof => return Err("git ended a request early".to_string()),
            }
        }
        let url = format!("{}{service}", self.target.base);
        let response = if ended {
            self.post_buffered(service, &url, &request, framed)?
        } else {
            // A retry cannot rewind a stream, so find out first if credentials
            // are needed, with an empty request, as Git's own helper does.
            self.post_buffered(service, &url, b"0000", false)?;
            let agent = self.agent()?;
            let mut stream = Streamed {
                pending: request,
                sent: 0,
                input,
                framed,
                ended: false,
            };
            let builder = self.post_builder(&agent, service, &url, framed);
            let response = builder
                .send(SendBody::from_reader(&mut stream))
                .map_err(|e| self.unable(e))?;
            let status = response.status().as_u16();
            if !(200..300).contains(&status) {
                return Err(format!("RPC failed; HTTP {status}"));
            }
            response
        };
        self.approve();
        let mut reader = Paced::new(response.into_body().into_reader(), self.settings.low_speed);
        let mut buffer = vec![0_u8; 64 * 1024];
        let mut wrote = false;
        loop {
            let read = reader.read(&mut buffer).map_err(|e| self.unable(e))?;
            if read == 0 {
                break;
            }
            wrote = true;
            let bytes = buffer.get(..read).unwrap_or(&[]);
            if framed {
                check.feed(bytes)?;
            }
            write_out(output, bytes)?;
        }
        if !wrote {
            return Err("RPC failed; the server sent an empty response".to_string());
        }
        Ok(())
    }

    fn post_builder(
        &self,
        agent: &Agent,
        service: &str,
        url: &str,
        framed: bool,
    ) -> ureq::RequestBuilder<ureq::typestate::WithBody> {
        let mut builder = agent
            .post(url)
            .header("Content-Type", &format!("application/x-{service}-request"))
            .header("Accept", &format!("application/x-{service}-result"));
        if framed {
            builder = builder.header("Git-Protocol", "version=2");
        }
        if let Some(authorization) = self.authorization() {
            builder = builder.header("Authorization", &authorization);
        }
        builder
    }

    fn post_buffered(
        &mut self,
        service: &str,
        url: &str,
        body: &[u8],
        framed: bool,
    ) -> Result<Response, String> {
        loop {
            let agent = self.agent()?;
            let response = self
                .post_builder(&agent, service, url, framed)
                .send(body)
                .map_err(|e| self.unable(e))?;
            let status = response.status().as_u16();
            if status == 401 {
                self.reauthenticate(offers_basic(&response))?;
                continue;
            }
            if !(200..300).contains(&status) {
                show_message(response, self.settings.low_speed);
                return Err(format!("RPC failed; HTTP {status}"));
            }
            return Ok(response);
        }
    }

    fn push(
        &mut self,
        specs: &[String],
        options: &Options,
        output: &mut impl Write,
    ) -> Result<(), String> {
        let (v2, advertisement) = {
            let discovery = self.discover("git-receive-pack", options.verbosity)?;
            (discovery.v2, Arc::clone(&discovery.body))
        };
        if v2 {
            return Err("invalid server response: a v2 advertisement for a push".to_string());
        }
        let mut child = Command::new("git")
            .args(send_pack_arguments(options, &self.target.base))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("run git send-pack: {e}"))?;
        let result = self.drive_send_pack(&mut child, specs, advertisement.as_slice(), output);
        drop(child.stdin.take());
        let waited = child.wait().map_err(|e| format!("wait for send-pack: {e}"));
        result?;
        // send-pack has told Git why, through its status or on stderr.
        if !waited?.success() {
            return Err(String::new());
        }
        Ok(())
    }

    fn drive_send_pack(
        &mut self,
        child: &mut std::process::Child,
        specs: &[String],
        advertisement: &[u8],
        output: &mut impl Write,
    ) -> Result<(), String> {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "send-pack has no stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "send-pack has no stdout".to_string())?;
        let mut from_pack = BufReader::new(stdout);
        let write_error = |e: io::Error| format!("write to git send-pack: {e}");
        let mut preamble = Vec::new();
        for spec in specs {
            let line = format!("{spec}\n");
            preamble.extend_from_slice(format!("{:04x}", line.len() + 4).as_bytes());
            preamble.extend_from_slice(line.as_bytes());
        }
        preamble.extend_from_slice(b"0000");
        stdin.write_all(&preamble).map_err(write_error)?;
        stdin.write_all(advertisement).map_err(write_error)?;
        stdin.flush().map_err(write_error)?;
        loop {
            let mut request = Vec::new();
            match read_frame(&mut from_pack, false, &mut request)
                .map_err(|e| format!("read from git send-pack: {e}"))?
            {
                Frame::Flush | Frame::Eof => break,
                Frame::Other => {}
            }
            let mut check = FrameCheck::default();
            self.exchange(
                "git-receive-pack",
                request,
                false,
                &mut from_pack,
                false,
                &mut stdin,
                &mut check,
            )?;
        }
        drop(stdin);
        // What is left is send-pack's status for Git.
        let mut status = Vec::new();
        from_pack
            .read_to_end(&mut status)
            .map_err(|e| format!("read from git send-pack: {e}"))?;
        write_out(output, &status)
    }
}

/// A proxy this client can use, or why not. The value may hold a password, so
/// no error says what it was.
fn usable_proxy(value: &str) -> Result<Proxy, String> {
    let proxy = Proxy::new(value)
        .map_err(|_| "the configured HTTP proxy is not a valid proxy URL".to_string())?;
    if !matches!(proxy.protocol(), ProxyProtocol::Http | ProxyProtocol::Https) {
        return Err("td's HTTP transport does not support a SOCKS proxy".to_string());
    }
    // Curl and this client's defaults differ, so saying one is the only safe way.
    if proxy.uri().port_u16().is_none() {
        return Err("the configured HTTP proxy names no port".to_string());
    }
    Ok(proxy)
}

fn send_pack_arguments(options: &Options, base: &str) -> Vec<String> {
    let mut args = vec![
        "send-pack".to_string(),
        "--stateless-rpc".to_string(),
        "--helper-status".to_string(),
        "--thin".to_string(),
    ];
    if options.dry_run {
        args.push("--dry-run".to_string());
    }
    if let Some(signed) = options.signed {
        args.push(signed.to_string());
    }
    if options.atomic {
        args.push("--atomic".to_string());
    }
    if options.verbosity == 0 {
        args.push("--quiet".to_string());
    } else if options.verbosity > 1 {
        args.push("--verbose".to_string());
    }
    for option in &options.push_options {
        args.push(format!("--push-option={option}"));
    }
    args.push(if options.progress {
        "--progress".to_string()
    } else {
        "--no-progress".to_string()
    });
    args.extend(options.leases.iter().cloned());
    args.push(base.to_string());
    if options.force_if_includes {
        args.push("--force-if-includes".to_string());
    }
    args.push("--stdin".to_string());
    args
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

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn pkt(line: &str) -> String {
        format!("{:04x}{line}", line.len() + 4)
    }

    #[test]
    fn a_target_never_keeps_credentials_in_its_base() {
        let target = Target::parse("https://me:p%40ss@example.test:8443/a/b.git").unwrap();
        assert_eq!(target.base, "https://example.test:8443/a/b.git/");
        assert_eq!(target.user.as_deref(), Some("me"));
        assert_eq!(target.password.as_deref(), Some("p@ss"));
        assert_eq!(target.origin(), "https://example.test:8443");
        assert_eq!(target.host, "example.test");
    }

    #[test]
    fn a_target_refuses_what_it_cannot_represent_without_echoing_it() {
        for url in [
            "ftp://me:secret@example.test/a",
            "https://me:secret@example.test/a?x=y",
            "https://me:secret@example.test/a#f",
            "https://me:%zz@example.test/a",
            "https://me:%0asecret@example.test/a",
        ] {
            let error = Target::parse(url).unwrap_err();
            assert!(!error.contains("secret"), "{error}");
        }
    }

    #[test]
    fn a_redirect_moves_the_base_and_cannot_downgrade() {
        let target = Target::parse("https://a.test/r.git").unwrap();
        let tail = "info/refs?service=git-upload-pack";
        let requested = format!("{}{tail}", target.base);
        let next = target
            .redirected(&requested, &format!("/moved/r.git/{tail}"), tail)
            .unwrap();
        assert_eq!(next.base, "https://a.test/moved/r.git/");
        let next = target
            .redirected(&requested, &format!("https://b.test/r.git/{tail}"), tail)
            .unwrap();
        assert_eq!(next.origin(), "https://b.test");
        let next = target
            .redirected(&requested, &format!("//c.test/r.git/{tail}"), tail)
            .unwrap();
        assert_eq!(next.origin(), "https://c.test");
        let next = target
            .redirected(&requested, &format!("sub/{tail}"), tail)
            .unwrap();
        assert_eq!(next.base, "https://a.test/r.git/info/sub/");
        for location in [
            format!("http://a.test/r.git/{tail}"),
            format!("ftp://a.test/r.git/{tail}"),
            format!("https://me:pw@a.test/r.git/{tail}"),
            format!("../r.git/{tail}"),
            "https://a.test/elsewhere".to_string(),
        ] {
            assert!(
                target.redirected(&requested, &location, tail).is_err(),
                "{location}"
            );
        }
        // Up from plain http is the one scheme change allowed.
        let plain = Target::parse("http://a.test/r.git").unwrap();
        let next = plain
            .redirected(&requested, &format!("https://a.test/r.git/{tail}"), tail)
            .unwrap();
        assert!(next.https);
    }

    #[test]
    fn git_config_output_parses_with_and_without_values() {
        assert_eq!(
            parse_config(b"http.sslcainfo\n/x/ca.pem\0http.SSLVerify\0"),
            vec![
                ("http.sslcainfo".to_string(), Some("/x/ca.pem".to_string())),
                ("http.sslverify".to_string(), None),
            ]
        );
    }

    fn settings(config: &[(&str, Option<&str>)]) -> Result<Settings, String> {
        let config: Vec<_> = config
            .iter()
            .map(|(key, value)| (key.to_string(), value.map(str::to_string)))
            .collect();
        let target = Target::parse("https://a.test/r.git").unwrap();
        Settings::new(&target, &config, &[], &no_env)
    }

    #[test]
    fn a_setting_it_cannot_honor_is_refused_by_name() {
        for key in [
            "http.sslcert",
            "http.sslkey",
            "http.pinnedpubkey",
            "http.cookiefile",
            "http.extraheader",
            "http.emptyauth",
            "http.proactiveauth",
            "http.somethingnew",
        ] {
            let error = settings(&[(key, Some("x"))]).unwrap_err();
            assert!(error.contains(key), "{error}");
        }
        let error = settings(&[("http.sslverify", Some("false"))]).unwrap_err();
        assert!(error.contains("sslVerify"), "{error}");
        assert!(settings(&[("http.sslverify", Some("true"))]).is_ok());
        assert!(settings(&[("http.sslverify", None)]).is_ok());
        for key in HARMLESS_CONFIG {
            assert!(settings(&[(key, Some("1"))]).is_ok(), "{key}");
        }
    }

    #[test]
    fn settings_prefer_the_environment_for_trust_and_read_the_low_speed() {
        assert_eq!(settings(&[]).unwrap().ca_bundle, DEFAULT_CA_BUNDLE);
        assert_eq!(settings(&[]).unwrap().low_speed, None);
        assert_eq!(settings(&[]).unwrap().post_buffer, POST_BUFFER);
        let got = settings(&[
            ("http.sslcainfo", Some("/x")),
            ("http.lowspeedlimit", Some("1")),
            ("http.lowspeedtime", Some("30")),
        ])
        .unwrap();
        assert_eq!(got.ca_bundle, "/x");
        assert_eq!(
            got.low_speed,
            Some(LowSpeed {
                limit: 1,
                time: Duration::from_secs(30)
            })
        );
        // A limit of nothing asks for no deadline.
        let got = settings(&[("http.lowspeedtime", Some("30"))]).unwrap();
        assert_eq!(got.low_speed, None);
        assert!(settings(&[("http.lowspeedtime", Some("soon"))]).is_err());
        let target = Target::parse("https://a.test/r.git").unwrap();
        let limited = [
            ("http.lowspeedlimit".to_string(), Some("1000".to_string())),
            ("http.lowspeedtime".to_string(), Some("60".to_string())),
        ];
        let low_speed = |value: &'static str| {
            let env =
                move |name: &str| (name == "GIT_HTTP_LOW_SPEED_LIMIT").then(|| value.to_string());
            Settings::new(&target, &limited, &[], &env)
                .unwrap()
                .low_speed
        };
        let kept = Some(LowSpeed {
            limit: 1000,
            time: Duration::from_secs(60),
        });
        assert_eq!(low_speed(""), kept);
        assert_eq!(low_speed("0"), None);
        let config = [("http.sslcainfo".to_string(), Some("/x".to_string()))];
        let env = |name: &str| (name == "GIT_SSL_CAINFO").then(|| "/y".to_string());
        assert_eq!(
            Settings::new(&target, &config, &[], &env)
                .unwrap()
                .ca_bundle,
            "/y"
        );
    }

    #[test]
    fn settings_read_as_git_writes_them() {
        assert_eq!(parse_number("x", Some("7")).unwrap(), 7);
        assert_eq!(parse_number("x", Some("2k")).unwrap(), 2048);
        assert_eq!(parse_number("x", Some("3M")).unwrap(), 3 << 20);
        assert_eq!(parse_number("x", Some("1g")).unwrap(), 1 << 30);
        for bad in ["", "k", "-1", "1t", "99999999999999999999g"] {
            assert!(parse_number("x", Some(bad)).is_err(), "{bad}");
        }
        let got = settings(&[("http.postbuffer", Some("10m"))]).unwrap();
        assert_eq!(got.post_buffer, 10 << 20);
        let got = settings(&[("http.postbuffer", Some("1"))]).unwrap();
        assert_eq!(got.post_buffer, MAX_PACKET + 4);
        // An empty value is a reset, not a request for the feature.
        assert!(settings(&[("http.extraheader", Some(""))]).is_ok());
        // An empty cookie file turns curl's cookie engine on.
        assert!(settings(&[("http.cookiefile", Some(""))]).is_err());
        // A boolean's empty value is false, where false asks for something.
        assert!(settings(&[("http.sslverify", Some(""))]).is_err());
        let got = settings(&[("http.followredirects", Some(""))]).unwrap();
        assert!(!got.follow_redirects);
        assert_eq!(
            settings(&[("http.sslcainfo", Some(""))]).unwrap().ca_bundle,
            DEFAULT_CA_BUNDLE
        );
        // A bare key is a boolean's true: it asks.
        assert!(settings(&[("http.emptyauth", None)]).is_err());

        let home = |name: &str| (name == "HOME").then(|| "/h".to_string());
        assert_eq!(expand_path("k", "~/ca", &home).unwrap(), "/h/ca");
        assert_eq!(expand_path("k", "/ca", &home).unwrap(), "/ca");
        assert!(expand_path("k", "~/ca", &no_env).is_err());
        assert!(expand_path("k", "~other/ca", &home).is_err());
        assert!(expand_path("k", ":(optional)/ca", &home).is_err());
        assert!(expand_path("k", "%(prefix)/ca", &home).is_err());
    }

    #[test]
    fn a_proxy_this_client_cannot_use_is_refused_before_it_could_panic() {
        assert!(usable_proxy("http://proxy.test:3128").is_ok());
        assert!(usable_proxy("proxy.test:3128").is_ok());
        assert!(usable_proxy("https://proxy.test:3128").is_ok());
        let secret = "http://me:s3cret@[bad";
        assert!(!usable_proxy(secret).err().unwrap().contains("s3cret"));
        for socks in [
            "socks5://proxy.test:1080",
            "socks5h://proxy.test:1080",
            "socks4://proxy.test:1080",
        ] {
            assert!(
                usable_proxy(socks).err().unwrap().contains("SOCKS"),
                "{socks}"
            );
        }
        for portless in ["http://proxy.test", "proxy.test"] {
            assert!(
                usable_proxy(portless).err().unwrap().contains("no port"),
                "{portless}"
            );
        }
    }

    #[test]
    fn the_low_speed_limit_is_a_rate_not_a_stall() {
        let low_speed = Some(LowSpeed {
            limit: 1000,
            time: Duration::from_secs(60),
        });
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);
        let mut paced = Paced::new((), low_speed);
        paced.since = start;
        // A trickle often enough for any per-read timeout, and below the rate.
        for secs in [15, 30, 45, 59] {
            paced.note(1, at(secs)).unwrap();
        }
        assert_eq!(
            paced.note(1, at(60)).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        // Plenty in the window starts the next one clean.
        let mut paced = Paced::new((), low_speed);
        paced.since = start;
        paced.note(60_000, at(60)).unwrap();
        paced.note(1, at(90)).unwrap();
        paced.note(60_000, at(120)).unwrap();
        // Exactly at the limit, however late each read lands: upload-pack's
        // keepalives against a limit of one for thirty seconds.
        let mut paced = Paced::new(
            (),
            Some(LowSpeed {
                limit: 1,
                time: Duration::from_secs(30),
            }),
        );
        paced.since = start;
        for tick in 1..=60_u64 {
            paced
                .note(5, start + Duration::from_millis(tick * 5_010))
                .unwrap();
        }
        // No limit, no opinion.
        let mut paced = Paced::new((), None);
        paced.since = start;
        paced.note(1, at(100_000)).unwrap();
    }

    #[test]
    fn a_same_origin_redirect_keeps_the_url_s_credentials_and_only_that() {
        let target = Target::parse("https://me:pw@a.test/r").unwrap();
        let tail = "info/refs?service=git-upload-pack";
        let requested = format!("{}{tail}", target.base);
        let next = target
            .redirected(&requested, &format!("/r.git/{tail}"), tail)
            .unwrap();
        assert_eq!(next.base, "https://a.test/r.git/");
        assert_eq!(next.user.as_deref(), Some("me"));
        assert_eq!(next.password.as_deref(), Some("pw"));
        // The same origin written another way.
        let next = target
            .redirected(
                &requested,
                &format!("https://A.test:443/r.git/{tail}"),
                tail,
            )
            .unwrap();
        assert_eq!(next.password.as_deref(), Some("pw"));
        for elsewhere in [
            format!("https://b.test/r.git/{tail}"),
            format!("https://a.test:8443/r.git/{tail}"),
        ] {
            let next = target.redirected(&requested, &elsewhere, tail).unwrap();
            assert_eq!(next.user, None, "{elsewhere}");
            assert_eq!(next.password, None, "{elsewhere}");
        }
        let plain = Target::parse("http://me:pw@a.test/r").unwrap();
        let requested = format!("{}{tail}", plain.base);
        let next = plain
            .redirected(&requested, &format!("https://a.test/r/{tail}"), tail)
            .unwrap();
        assert!(next.https);
        assert_eq!(next.password, None);
    }

    #[test]
    fn a_refused_environment_names_itself() {
        let env = |name: &str| (name == "GIT_SSL_NO_VERIFY").then(|| "1".to_string());
        assert!(refuse_environment(&env)
            .unwrap_err()
            .contains("GIT_SSL_NO_VERIFY"));
        let env = |name: &str| (name == "GIT_SMART_HTTP").then(|| "0".to_string());
        assert!(refuse_environment(&env).is_err());
        assert!(refuse_environment(&no_env).is_ok());
    }

    #[test]
    fn the_proxy_follows_git_and_curl() {
        let https = Target::parse("https://a.test/r").unwrap();
        let http = Target::parse("http://a.test/r").unwrap();
        let env = |name: &str| match name {
            "HTTP_PROXY" => Some("http://cgi".to_string()),
            "https_proxy" => Some("http://secure".to_string()),
            "ALL_PROXY" => Some("http://all".to_string()),
            _ => None,
        };
        assert_eq!(select_proxy(&https, None, &env).unwrap(), "http://secure");
        // HTTP_PROXY is never read: a CGI request's Proxy header sets it.
        assert_eq!(select_proxy(&http, None, &env).unwrap(), "http://all");
        // Configuration beats the environment, and empty means none.
        assert_eq!(
            select_proxy(&https, Some("http://set".to_string()), &env).unwrap(),
            "http://set"
        );
        assert_eq!(select_proxy(&https, Some(String::new()), &env), None);
        let bypass = |name: &str| match name {
            "https_proxy" => Some("http://secure".to_string()),
            "no_proxy" => Some("other.test, .a.test".to_string()),
            _ => None,
        };
        assert_eq!(select_proxy(&https, None, &bypass), None);
    }

    #[test]
    fn no_proxy_matches_a_domain_not_a_suffix() {
        assert!(no_proxy_matches("a.test", "a.test"));
        assert!(no_proxy_matches("a.test", "sub.a.test"));
        assert!(no_proxy_matches(".a.test", "sub.a.test"));
        assert!(no_proxy_matches("*", "anything"));
        assert!(no_proxy_matches("localhost,127.0.0.1,::1", "[::1]"));
        assert!(no_proxy_matches("[::1]", "[::1]"));
        assert!(!no_proxy_matches("a.test", "bad-a.test"));
        assert!(!no_proxy_matches("", "a.test"));
        assert!(!no_proxy_matches(",,", "a.test"));
    }

    #[test]
    fn options_answer_as_git_expects() {
        let mut options = Options::new();
        let mut held = settings(&[]).unwrap();
        for (line, answer) in [
            ("verbosity 2", "ok\n"),
            ("verbosity x", "error invalid value\n"),
            ("progress true", "ok\n"),
            ("progress false", "ok\n"),
            ("progress maybe", "error invalid value\n"),
            ("dry-run true", "ok\n"),
            ("atomic true", "ok\n"),
            ("depth 1", "ok\n"),
            ("filter blob:none", "ok\n"),
            ("family ipv4", "ok\n"),
            ("family carrier-pigeon", "error invalid value\n"),
            ("pushcert if-asked", "ok\n"),
            ("object-format true", "ok\n"),
            ("object-format sha256", "error invalid value\n"),
            ("cas refs/heads/a:\"", "ok\n"),
            ("cas \"bad", "error invalid value\n"),
            ("push-option \"a\\tb\"", "ok\n"),
            ("something-new x", "unsupported\n"),
        ] {
            assert_eq!(options.set(line, &mut held), answer, "{line}");
        }
        assert_eq!(options.verbosity, 2);
        assert!(!options.progress);
        assert!(options.dry_run && options.atomic && options.object_format);
        assert_eq!(options.signed, Some("--signed=if-asked"));
        assert_eq!(options.push_options, ["a\tb"]);
        assert_eq!(options.leases, ["--force-with-lease=refs/heads/a:\""]);
        assert_eq!(held.family, ureq::config::IpFamily::Ipv4Only);
        assert_eq!(options.set("dry-run false", &mut held), "ok\n");
        assert!(!options.dry_run);
    }

    #[test]
    fn unquote_undoes_c_style() {
        assert_eq!(unquote("plain").unwrap(), "plain");
        assert_eq!(unquote(r#""a\"b\\c\n\101""#).unwrap(), "a\"b\\c\nA");
        for bad in ["\"open", "\"tail\"x", "\"\\q\"", "\"\\7\""] {
            assert_eq!(unquote(bad), None, "{bad}");
        }
    }

    #[test]
    fn packets_parse_and_bad_framing_fails() {
        let bytes = format!("{}000100000002", pkt("hi\n"));
        let mut at = 0;
        assert_eq!(
            next_packet(bytes.as_bytes(), &mut at).unwrap(),
            Packet::Data(b"hi\n")
        );
        assert_eq!(
            next_packet(bytes.as_bytes(), &mut at).unwrap(),
            Packet::Delim
        );
        assert_eq!(
            next_packet(bytes.as_bytes(), &mut at).unwrap(),
            Packet::Flush
        );
        assert_eq!(
            next_packet(bytes.as_bytes(), &mut at).unwrap(),
            Packet::ResponseEnd
        );
        for bad in ["", "000", "zzzz", "0003", "0009hi", "fff5"] {
            assert!(next_packet(bad.as_bytes(), &mut 0).is_err(), "{bad}");
        }
    }

    fn advertisement(service: &str, rest: &str) -> Result<Discovery, String> {
        let body = format!("{}0000{rest}", pkt(&format!("# service={service}\n")));
        parse_advertisement(
            service,
            &format!("application/x-{service}-advertisement"),
            body.into_bytes(),
        )
    }

    #[test]
    fn an_advertisement_says_its_version_and_keeps_git_s_part() {
        let v2 = format!("{}{}0000", pkt("version 2\n"), pkt("agent=x\n"));
        let found = advertisement("git-upload-pack", &v2).unwrap();
        assert!(found.v2);
        assert_eq!(found.body.as_slice(), v2.as_bytes());
        // Without the service header, as protocol v2 allows.
        let found = parse_advertisement(
            "git-upload-pack",
            "application/x-git-upload-pack-advertisement",
            v2.clone().into_bytes(),
        )
        .unwrap();
        assert!(found.v2);
        assert_eq!(found.body.as_slice(), v2.as_bytes());
    }

    #[test]
    fn a_ref_listing_shows_only_proper_refs() {
        let oid = "a".repeat(40);
        let zero = "0".repeat(40);
        let rest = format!(
            "{}{}{}{}{}{}{}0000",
            pkt(&format!("{oid} HEAD\0report-status object-format=sha1\n")),
            pkt(&format!("{oid} refs/heads/main\n")),
            pkt(&format!("{oid} refs/tags/v1\n")),
            pkt(&format!("{oid} refs/tags/v1^{{}}\n")),
            pkt(&format!("{zero} .have\n")),
            pkt(&format!("{oid} refs/heads/a\tb\n")),
            pkt(&format!("shallow {oid}\n")),
        );
        let found = advertisement("git-receive-pack", &rest).unwrap();
        assert!(!found.v2);
        assert_eq!(
            found.ref_listing(true),
            format!(":object-format sha1\n{oid} refs/heads/main\n{oid} refs/tags/v1\n\n")
        );
        // A name that cannot be spoken costs that ref, not the whole push.
        let mut odd = format!("{:04x}{oid} refs/heads/", 4 + 40 + 12 + 1 + 1).into_bytes();
        odd.extend_from_slice(b"\xff\n");
        odd.extend_from_slice(pkt(&format!("{oid} refs/heads/ok\n")).as_bytes());
        odd.extend_from_slice(b"0000");
        let (refs, _) = parse_refs(&odd).unwrap();
        assert_eq!(refs, [(oid.clone(), "refs/heads/ok".to_string())]);
        let empty = format!(
            "{}0000",
            pkt(&format!("{zero} capabilities^{{}}\0object-format=sha256\n"))
        );
        let found = advertisement("git-receive-pack", &empty).unwrap();
        assert_eq!(found.ref_listing(true), ":object-format sha256\n\n");
        assert_eq!(found.ref_listing(false), "\n");
    }

    #[test]
    fn a_bad_advertisement_is_refused() {
        let service = "git-upload-pack";
        let kind = "application/x-git-upload-pack-advertisement";
        let dumb = parse_advertisement(service, "text/plain", b"a\trefs/heads/x\n".to_vec());
        assert!(dumb.unwrap_err().contains("smart HTTP"));
        let error = parse_advertisement(service, kind, pkt("ERR no access\n").into_bytes());
        assert_eq!(error.unwrap_err(), "remote error: no access");
        for body in [
            "0000".to_string(),
            pkt("something else\n"),
            pkt("# service=git-receive-pack\n"),
            format!(
                "{}0000{}",
                pkt("# service=git-upload-pack\n"),
                pkt("xyz q\n")
            ),
            format!("{}0000", pkt("# service=git-upload-pack\n")),
        ] {
            assert!(
                parse_advertisement(service, kind, body.clone().into_bytes()).is_err(),
                "{body}"
            );
        }
        let cut = pkt(&format!("{} refs/heads/a\n", "a".repeat(40)));
        assert!(advertisement("git-receive-pack", &cut).is_err());
        let error = advertisement("git-upload-pack", &pkt("ERR gone\n")).unwrap_err();
        assert_eq!(error, "remote error: gone");
        // A v2 advertisement with no flush would leave Git waiting forever.
        let open = format!("{}{}", pkt("version 2\n"), pkt("agent=x\n"));
        assert!(advertisement("git-upload-pack", &open).is_err());
        let extra = format!("{}0000{}", pkt("version 2\n"), pkt("agent=x\n"));
        assert!(advertisement("git-upload-pack", &extra).is_err());
        let long = format!("{}{}0000", pkt("version 2\n"), pkt("x").repeat(300_000));
        assert!(advertisement("git-upload-pack", &long).is_err());
        let forged = format!("{}0000", pkt(&format!("{} refs/a\0x\n", "z".repeat(40))));
        assert!(advertisement("git-receive-pack", &forged).is_err());
    }

    #[test]
    fn a_refname_cannot_forge_a_helper_line() {
        assert!(valid_refname("refs/heads/feature/x-1.2"));
        for bad in [
            "refs/heads/a b",
            "refs/heads/a\nb",
            "refs/heads/a:b",
            "refs/heads/a..b",
            "refs/heads/.a",
            "refs/heads/a.lock",
            "refs/heads/a//b",
            "refs/heads/a/",
            "refs/heads/a.",
            "refs/heads/a@{1}",
            "refs/heads/\u{7f}",
        ] {
            assert!(!valid_refname(bad), "{bad:?}");
        }
    }

    #[test]
    fn frames_keep_or_strip_their_lengths() {
        let stream = format!("{}0001{}0000", pkt("one"), pkt("two"));
        let mut input = stream.as_bytes();
        let mut framed = Vec::new();
        for want in [
            Frame::Other,
            Frame::Other,
            Frame::Other,
            Frame::Flush,
            Frame::Eof,
        ] {
            assert_eq!(read_frame(&mut input, true, &mut framed).unwrap(), want);
        }
        assert_eq!(framed, stream.as_bytes());
        let mut input = stream.as_bytes();
        let mut stripped = Vec::new();
        for want in [Frame::Other, Frame::Other, Frame::Other, Frame::Flush] {
            assert_eq!(read_frame(&mut input, false, &mut stripped).unwrap(), want);
        }
        assert_eq!(stripped, b"onetwo");
        for bad in ["00", "zzzz", "0002", "0003", "0009ab", "fff5"] {
            assert!(
                read_frame(&mut bad.as_bytes(), true, &mut Vec::new()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_streamed_body_ends_at_its_flush_and_leaves_the_rest() {
        let stream = format!("{}0000{}", pkt("two"), pkt("after"));
        let mut input = stream.as_bytes();
        let mut streamed = Streamed {
            pending: b"one".to_vec(),
            sent: 0,
            input: &mut input,
            framed: false,
            ended: false,
        };
        let mut got = Vec::new();
        streamed.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"onetwo");
        assert_eq!(input, pkt("after").as_bytes());
        let mut short: &[u8] = b"";
        let mut streamed = Streamed {
            pending: Vec::new(),
            sent: 0,
            input: &mut short,
            framed: false,
            ended: false,
        };
        assert!(streamed.read_to_end(&mut Vec::new()).is_err());
    }

    #[test]
    fn the_frame_check_holds_across_reads_and_catches_a_cut() {
        let whole = format!("{}{}0000", pkt("hello"), pkt("world"));
        for split in 0..whole.len() {
            let (left, right) = whole.split_at(split);
            let mut check = FrameCheck::default();
            check.feed(left.as_bytes()).unwrap();
            check.feed(right.as_bytes()).unwrap();
            check.finish().unwrap();
        }
        let mut check = FrameCheck::default();
        check.feed(b"0009he").unwrap();
        assert!(check.finish().is_err());
        let mut check = FrameCheck::default();
        check.feed(b"00").unwrap();
        assert!(check.finish().is_err());
        for bad in ["zzzz", "0002", "0003", "fff5"] {
            assert!(FrameCheck::default().feed(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded);
        }
    }

    #[test]
    fn a_server_cannot_write_control_characters_to_the_terminal() {
        assert_eq!(printable(b"a\x1b[31mb\tc"), "a?[31mb\tc");
        assert_eq!(
            printable_lines(b"  one\n\x07two\n "),
            vec!["one".to_string(), "?two".to_string()]
        );
        assert!(printable_lines(b" \n ").is_empty());
    }

    #[test]
    fn send_pack_gets_what_the_options_asked_for() {
        let mut options = Options::new();
        options.progress = false;
        let args = send_pack_arguments(&options, "https://a.test/r/");
        assert_eq!(
            args,
            [
                "send-pack",
                "--stateless-rpc",
                "--helper-status",
                "--thin",
                "--no-progress",
                "https://a.test/r/",
                "--stdin",
            ]
        );
        options.dry_run = true;
        options.atomic = true;
        options.verbosity = 0;
        options.force_if_includes = true;
        options.signed = Some("--signed=yes");
        options.push_options.push("ci.skip".to_string());
        options.leases.push("--force-with-lease=a:b".to_string());
        let args = send_pack_arguments(&options, "https://a.test/r/");
        for want in [
            "--dry-run",
            "--atomic",
            "--quiet",
            "--signed=yes",
            "--push-option=ci.skip",
            "--force-with-lease=a:b",
            "--force-if-includes",
        ] {
            assert!(args.iter().any(|arg| arg == want), "{want}");
        }
    }
}
