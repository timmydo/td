// td-curl — a small HTTP(S) download client, also linked as /bin/curl so that the
// way people ask for a download keeps working. It is not curl and says so: every
// message and `--version` say td-curl and that it is not curl, and an option it
// does not list in `--help` is an error, never ignored. An option it accepts is
// true of what it does.
//
// It only ever GETs. It reads no `.curlrc`, follows at most `MAX_REDIRECTS`,
// refuses https to http, and sends `-H` headers only to the origin that was named.
// A URL may not carry credentials; `-H` sends them, under that same rule.
//
// A response that can carry a body has to show how long it is. ureq reads a connection's end, reset and
// a missing TLS close_notify included, as the end of a body with no length, so a
// download cut short there would look whole: `curl URL | sh` running half a script.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)]
use std::io::{self, IsTerminal, Read, Write};
use std::time::{Duration, Instant};

use ureq::config::AutoHeaderValue;
use ureq::Agent;

use crate::client;

/// What every message starts with.
pub(crate) const NAME: &str = "td-curl (not curl)";
const MAX_REDIRECTS: u32 = 10;
/// A year: past any real timeout, and short of what an Instant can add.
const MAX_SECONDS: f64 = 31_536_000.0;
/// These take their value as the next argument, as curl's do.
const VALUE_OPTIONS: &[&str] = &[
    "output",
    "header",
    "user-agent",
    "max-time",
    "connect-timeout",
    "cacert",
    "proto",
];

const HELP: &str = "\
td-curl: a small HTTP(S) download client. It is not curl: it only GETs, reads no
.curlrc, and an option not listed here is an error.

usage: td-curl [options] URL

  -f, --fail                exit 22 with no output on an HTTP error
  -s, --silent              say nothing on failure (there is never a progress meter)
  -S, --show-error          say why on failure, even with -s
  -L, --location            follow redirects, at most ten; https to http is refused
                            and -H headers stay with the origin that was named
  -o, --output FILE         write to FILE; - is standard output
  -O, --remote-name         write to the URL's last path segment
  -H, --header 'NAME: VALUE'  send a header; may be repeated
  -A, --user-agent TEXT     the User-Agent; empty sends none
      --proto =https        refuse anything but https, redirects included
      --tlsv1.2             accepted because TLS 1.2 is already the minimum
      --connect-timeout S   seconds to connect, TLS included, shared by a host's
                            addresses, for each redirect (default 60)
  -m, --max-time S          seconds for the whole operation (default none)
      --cacert FILE         trust this bundle, not the system's
  -V, --version             say what this is
  -h, --help                this text

Trust is --cacert, then CURL_CA_BUNDLE, then SSL_CERT_FILE, then
/etc/ssl/certs/ca-certificates.crt. With none of the first three named, a set
SSL_CERT_DIR is refused, since a directory is not read.

A proxy comes from https_proxy, HTTPS_PROXY, http_proxy, all_proxy, ALL_PROXY,
no_proxy and NO_PROXY, and is always tunneled with CONNECT; SOCKS is refused.

A response that can carry a body must give its length or be chunked, or it is
refused: otherwise a download cut short could not be told from a whole one. A
connection silent for five minutes fails. Output holding a NUL byte is refused
when standard output is a terminal.

exit: 0 done, 1 failed, 2 usage, 22 HTTP error with -f, 23 could not write,
28 timed out.
";

pub fn run(args: &[String]) {
    let lookup = |name: &str| std::env::var(name).ok();
    let code = match parse(args.get(1..).unwrap_or(&[])) {
        Err(usage) => {
            say(&format!("{usage}; `td-curl --help` lists what it does"));
            2
        }
        Ok(Parsed::Help) => print_exit(HELP),
        Ok(Parsed::Version) => print_exit(&version()),
        Ok(Parsed::Fetch(options)) => match fetch(&options, &lookup) {
            Ok(()) => 0,
            Err(failure) => {
                if !failure.message.is_empty() && (!options.silent || options.show_error) {
                    say(&format!("({}) {}", failure.code, failure.message));
                }
                failure.code
            }
        },
    };
    std::process::exit(code);
}

/// One line on stderr: a certificate's name can reach a message, so nothing
/// here may steer a terminal, and a closed stderr is no reason to panic.
fn say(message: &str) {
    let text = client::printable(message.as_bytes());
    let _ = writeln!(io::stderr(), "{NAME}: {text}");
}

fn print_exit(text: &str) -> i32 {
    let mut out = io::stdout();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => 0,
        Err(_) => 23,
    }
}

fn version() -> String {
    format!(
        "td-curl {} (td-net, rustls): not curl; `td-curl --help` lists what it does\n",
        env!("CARGO_PKG_VERSION")
    )
}

#[derive(Debug, PartialEq)]
enum Parsed {
    Help,
    Version,
    Fetch(Options),
}

#[derive(Debug, Default, PartialEq)]
struct Options {
    url: String,
    fail: bool,
    silent: bool,
    show_error: bool,
    location: bool,
    output: Option<String>,
    remote_name: bool,
    headers: Vec<(String, String)>,
    user_agent: Option<String>,
    https_only: bool,
    connect_timeout: Option<Duration>,
    max_time: Option<Duration>,
    cacert: Option<String>,
}

/// Only the option's own name is ever repeated back: what follows it, as in
/// `--user=me:secret`, stays out of the message.
fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut options = Options::default();
    let mut url = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if let Some(long) = arg.strip_prefix("--") {
            match long {
                "help" => return Ok(Parsed::Help),
                "version" => return Ok(Parsed::Version),
                "fail" => options.fail = true,
                "silent" => options.silent = true,
                "show-error" => options.show_error = true,
                "location" => options.location = true,
                "remote-name" => options.remote_name = true,
                "tlsv1.2" => {}
                "output" | "header" | "user-agent" | "max-time" => {
                    let value = next_value(&mut rest, arg)?;
                    let flag = match long {
                        "output" => 'o',
                        "header" => 'H',
                        "user-agent" => 'A',
                        _ => 'm',
                    };
                    set_value(&mut options, flag, value)?;
                }
                "connect-timeout" => {
                    let value = next_value(&mut rest, arg)?;
                    options.connect_timeout = Some(parse_seconds(arg, value)?);
                }
                "cacert" => options.cacert = Some(next_value(&mut rest, arg)?.to_string()),
                "proto" => {
                    if next_value(&mut rest, arg)? != "=https" {
                        return Err("--proto takes only =https".to_string());
                    }
                    options.https_only = true;
                }
                _ => {
                    let name = long.split('=').next().unwrap_or(long);
                    if VALUE_OPTIONS.contains(&name) {
                        return Err(format!(
                            "option --{name} takes its value as the next argument"
                        ));
                    }
                    return Err(format!("option --{name} is not supported"));
                }
            }
        } else if let Some(cluster) = arg.strip_prefix('-').filter(|text| !text.is_empty()) {
            for (at, flag) in cluster.char_indices() {
                match flag {
                    'h' => return Ok(Parsed::Help),
                    'V' => return Ok(Parsed::Version),
                    'f' => options.fail = true,
                    's' => options.silent = true,
                    'S' => options.show_error = true,
                    'L' => options.location = true,
                    'O' => options.remote_name = true,
                    'o' | 'H' | 'A' | 'm' => {
                        let attached = cluster.get(at + flag.len_utf8()..).unwrap_or("");
                        let value = if attached.is_empty() {
                            next_value(&mut rest, &format!("-{flag}"))?
                        } else {
                            attached
                        };
                        set_value(&mut options, flag, value)?;
                        break;
                    }
                    other => return Err(format!("option -{other} is not supported")),
                }
            }
        } else if url.is_some() {
            return Err("td-curl takes one URL".to_string());
        } else {
            url = Some(arg.clone());
        }
    }
    if options.output.is_some() && options.remote_name {
        return Err("-o and -O ask for two different places".to_string());
    }
    options.url = url.ok_or_else(|| "no URL".to_string())?;
    Ok(Parsed::Fetch(options))
}

fn next_value<'a>(rest: &mut std::slice::Iter<'a, String>, name: &str) -> Result<&'a str, String> {
    rest.next()
        .map(String::as_str)
        .ok_or_else(|| format!("option {name} needs a value"))
}

fn set_value(options: &mut Options, flag: char, value: &str) -> Result<(), String> {
    match flag {
        'o' => options.output = Some(value.to_string()),
        'A' => {
            if holds_control(value) {
                return Err("-A: a User-Agent may not hold a control character".to_string());
            }
            options.user_agent = Some(value.to_string());
        }
        'm' => options.max_time = Some(parse_seconds("-m", value)?),
        _ => options.headers.push(parse_header(value)?),
    }
    Ok(())
}

fn parse_seconds(name: &str, value: &str) -> Result<Duration, String> {
    value
        .parse::<f64>()
        .ok()
        .filter(|seconds| *seconds > 0.0 && *seconds <= MAX_SECONDS)
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
        .ok_or_else(|| format!("option {name} needs a positive number of seconds, up to a year"))
}

fn holds_control(value: &str) -> bool {
    value
        .bytes()
        .any(|byte| byte.is_ascii_control() && byte != b'\t')
}

/// `NAME: VALUE` only. curl's forms that remove a header or send an empty one
/// ask for something else, and a value is never repeated in the message.
fn parse_header(text: &str) -> Result<(String, String), String> {
    let refused = || "-H takes 'NAME: VALUE'".to_string();
    let (name, value) = text.split_once(':').ok_or_else(refused)?;
    let value = value.trim();
    let token = |byte: u8| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte);
    if name.is_empty() || !name.bytes().all(token) || value.is_empty() {
        return Err(refused());
    }
    if holds_control(value) {
        return Err("-H: a header value may not hold a control character".to_string());
    }
    Ok((name.to_string(), value.to_string()))
}

#[derive(Debug, PartialEq)]
struct Failure {
    code: i32,
    message: String,
}

fn failed(message: impl Into<String>) -> Failure {
    Failure {
        code: 1,
        message: message.into(),
    }
}

fn write_failed(what: &str, error: &io::Error) -> Failure {
    // A reader that went away, as `td-curl URL | head` has it, wants no message.
    let message = if error.kind() == io::ErrorKind::BrokenPipe {
        String::new()
    } else {
        format!("{what}: {error}")
    };
    Failure { code: 23, message }
}

fn transport_failed(error: ureq::Error) -> Failure {
    let code = match &error {
        ureq::Error::Timeout(_) => 28,
        ureq::Error::Io(error) if error.kind() == io::ErrorKind::TimedOut => 28,
        _ => 1,
    };
    Failure {
        code,
        message: error.to_string(),
    }
}

/// A body's read error, where ureq wraps its own timeout in an io::Error.
fn read_failed(error: &io::Error) -> Failure {
    let timed_out = error.kind() == io::ErrorKind::TimedOut
        || error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<ureq::Error>())
            .is_some_and(|inner| matches!(inner, ureq::Error::Timeout(_)));
    Failure {
        code: if timed_out { 28 } else { 1 },
        message: format!("the transfer did not finish: {error}"),
    }
}

/// A URL this client will ask for. The text never holds credentials, so a
/// message may quote what it is built from.
#[derive(Debug, PartialEq)]
struct Url {
    /// Origin, path and query: never a bare host for a redirect to resolve from.
    text: String,
    path: String,
    https: bool,
    host: String,
    origin: String,
}

impl Url {
    fn parse(text: &str) -> Result<Self, String> {
        // The server is not told a fragment, and curl does not send it either.
        let text = text.split('#').next().unwrap_or(text);
        let uri: ureq::http::Uri = text.parse().map_err(|_| "not a valid URL".to_string())?;
        let https = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => return Err("td-curl speaks only http:// and https://".to_string()),
        };
        let authority = uri
            .authority()
            .ok_or_else(|| "no host in the URL".to_string())?;
        if authority.as_str().contains('@') {
            return Err("a URL may not hold credentials; use -H".to_string());
        }
        let scheme = if https { "https" } else { "http" };
        let origin = format!("{scheme}://{}", authority.as_str());
        let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
        Ok(Self {
            text: format!("{origin}{}{query}", uri.path()),
            path: uri.path().to_string(),
            https,
            host: authority.host().to_string(),
            origin,
        })
    }

    fn origin_key(&self) -> String {
        client::origin_key(&self.origin, self.https)
    }

    /// The URL's last path segment, as `-O` names a file. It comes from what
    /// the person typed, never from a redirect.
    fn remote_name(&self) -> Result<String, String> {
        match self.path.rsplit('/').next().unwrap_or("") {
            // `-` would mean standard output, where -o is told so on purpose.
            "" | "." | ".." | "-" => Err("-O: the URL names no file".to_string()),
            name => Ok(name.to_string()),
        }
    }

    fn redirected(&self, location: &str) -> Result<Self, String> {
        let absolute = client::absolute_location(&self.origin, self.https, &self.text, location)
            .map_err(|reason| format!("the server {reason}"))?;
        let next = Self::parse(&absolute)
            .map_err(|_| "the server redirected to a URL td-curl will not follow".to_string())?;
        if self.https && !next.https {
            return Err(
                "the server redirected from https to plain http, which is refused".to_string(),
            );
        }
        Ok(next)
    }
}

fn named_bundle(options: &Options, lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    options
        .cacert
        .clone()
        .or_else(|| lookup("CURL_CA_BUNDLE").filter(|value| !value.is_empty()))
        .or_else(|| lookup("SSL_CERT_FILE").filter(|value| !value.is_empty()))
}

/// Whether a response says where its body ends, so that a connection's end
/// short of it is an error rather than the end of the body. Chunked is what ureq
/// itself takes to be chunked, and it reads 1xx, 204 and 3xx with neither header
/// as having no body at all.
fn says_its_length(response: &ureq::http::Response<ureq::Body>) -> bool {
    let headers = response.headers();
    let chunked = response.version() != ureq::http::Version::HTTP_10
        && headers
            .get("transfer-encoding")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|part| part.trim().eq_ignore_ascii_case("chunked"))
            });
    chunked
        || headers.contains_key("content-length")
        || matches!(response.status().as_u16(), 100..=199 | 204 | 300..=399)
}

fn agent_for(
    options: &Options,
    url: &Url,
    bundle: &str,
    remaining: Option<Duration>,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Agent, Failure> {
    let proxy = client::select_proxy(url.https, &url.host, None, lookup)
        .as_deref()
        .map(client::usable_proxy)
        .transpose()
        .map_err(failed)?;
    let tls = client::load_tls(bundle, url.https).map_err(failed)?;
    let connect = options.connect_timeout.unwrap_or(client::CONNECT_TIMEOUT);
    let user_agent: AutoHeaderValue = match options.user_agent.as_deref() {
        Some("") => AutoHeaderValue::None,
        Some(text) => text.into(),
        None => format!("td-curl/{}", env!("CARGO_PKG_VERSION"))
            .as_str()
            .into(),
    };
    Ok(Agent::new_with_config(
        Agent::config_builder()
            .proxy(proxy)
            .http_status_as_error(false)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .redirect_auth_headers(ureq::config::RedirectAuthHeaders::Never)
            .accept_encoding(AutoHeaderValue::None)
            .user_agent(user_agent)
            .tls_config(tls)
            .timeout_resolve(Some(connect.min(client::RESOLVE_TIMEOUT)))
            .timeout_connect(Some(connect))
            .timeout_per_read(Some(client::STALL_TIMEOUT))
            .timeout_per_write(Some(client::STALL_TIMEOUT))
            .timeout_global(remaining)
            .build(),
    ))
}

fn fetch(options: &Options, lookup: &dyn Fn(&str) -> Option<String>) -> Result<(), Failure> {
    let named = named_bundle(options, lookup);
    if named.is_none() && lookup("SSL_CERT_DIR").is_some_and(|value| !value.is_empty()) {
        return Err(failed(
            "SSL_CERT_DIR is set, and td-curl does not read a certificate directory; \
             name a file with --cacert",
        ));
    }
    let first = Url::parse(&options.url).map_err(failed)?;
    // A redirect can only stay on https from here: https to http is refused.
    if options.https_only && !first.https {
        return Err(failed("--proto =https refused a URL that is not https"));
    }
    let output = if options.remote_name {
        Some(
            first
                .remote_name()
                .map_err(|message| Failure { code: 23, message })?,
        )
    } else {
        options.output.clone()
    };
    let bundle = named.unwrap_or_else(|| client::DEFAULT_CA_BUNDLE.to_string());
    let started = Instant::now();
    let home = first.origin_key();
    let mut url = first;
    let mut hops = 0;
    let response = loop {
        let remaining = match options.max_time {
            None => None,
            Some(limit) => match limit.checked_sub(started.elapsed()) {
                Some(left) if !left.is_zero() => Some(left),
                _ => {
                    return Err(Failure {
                        code: 28,
                        message: "--max-time ran out".to_string(),
                    })
                }
            },
        };
        let agent = agent_for(options, &url, &bundle, remaining, lookup)?;
        let mut request = agent.get(&url.text);
        if url.origin_key() == home {
            for (name, value) in &options.headers {
                request = request.header(name, value);
            }
        }
        let response = request.call().map_err(transport_failed)?;
        let status = response.status().as_u16();
        let location = response.headers().get("location");
        // 300 may be an answer in itself; the others have nothing else to give.
        let moved =
            matches!(status, 301 | 302 | 303 | 307 | 308) || (status == 300 && location.is_some());
        if !options.location || !moved {
            break response;
        }
        hops += 1;
        if hops > MAX_REDIRECTS {
            return Err(failed("too many redirects"));
        }
        // Stopping here would hand the redirect's own page over as the download.
        let location = location
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| failed("the server redirected, and td-curl cannot read where to"))?;
        url = url.redirected(location).map_err(failed)?;
    };
    let status = response.status().as_u16();
    if options.fail && status >= 400 {
        return Err(Failure {
            code: 22,
            message: format!("The requested URL returned error: {status}"),
        });
    }
    if !says_its_length(&response) {
        return Err(failed(
            "the server did not say how long its answer is, so a whole one could not be \
             told from one cut short",
        ));
    }
    let mut body = response.into_body().into_reader();
    match output.as_deref() {
        None | Some("-") => {
            let mut out = io::stdout();
            let terminal = out.is_terminal();
            copy(&mut body, &mut out, "standard output", terminal)?;
            out.flush().map_err(|e| write_failed("standard output", &e))
        }
        // Only now, so a refused request leaves an existing file alone.
        Some(path) => {
            let mut file = std::fs::File::create(path).map_err(|e| write_failed(path, &e))?;
            copy(&mut body, &mut file, path, false)
        }
    }
}

fn copy(
    input: &mut impl Read,
    output: &mut impl Write,
    name: &str,
    terminal: bool,
) -> Result<(), Failure> {
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer).map_err(|e| read_failed(&e))?;
        if read == 0 {
            return Ok(());
        }
        let bytes = buffer.get(..read).unwrap_or(&[]);
        if terminal && bytes.contains(&0) {
            return Err(Failure {
                code: 23,
                message: "binary output would reach the terminal; use -o".to_string(),
            });
        }
        output
            .write_all(bytes)
            .map_err(|e| write_failed(name, &e))?;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn fetching(args: &[&str]) -> Result<Options, String> {
        let owned: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        match parse(&owned)? {
            Parsed::Fetch(options) => Ok(options),
            other => Err(format!("{other:?}")),
        }
    }

    #[test]
    fn the_usual_ways_of_asking_parse() {
        let got = fetching(&[
            "-fsSL",
            "--proto",
            "=https",
            "--tlsv1.2",
            "-o",
            "out",
            "-H",
            "Accept: x/y",
            "-HX-A: b",
            "-A",
            "me",
            "-m",
            "1.5",
            "--connect-timeout",
            "3",
            "--cacert",
            "ca.pem",
            "https://a.test/f",
        ])
        .unwrap();
        assert!(got.fail && got.silent && got.show_error && got.location && got.https_only);
        assert_eq!(got.output.as_deref(), Some("out"));
        assert_eq!(
            got.headers,
            [
                ("Accept".to_string(), "x/y".to_string()),
                ("X-A".to_string(), "b".to_string())
            ]
        );
        assert_eq!(got.user_agent.as_deref(), Some("me"));
        assert_eq!(got.max_time, Some(Duration::from_millis(1500)));
        assert_eq!(got.connect_timeout, Some(Duration::from_secs(3)));
        assert_eq!(got.cacert.as_deref(), Some("ca.pem"));
        assert_eq!(got.url, "https://a.test/f");
        // A value attached to a cluster's last flag.
        assert_eq!(
            fetching(&["-sofile", "https://a.test/"])
                .unwrap()
                .output
                .as_deref(),
            Some("file")
        );
        assert_eq!(parse(&["-V".to_string()]).unwrap(), Parsed::Version);
        assert_eq!(parse(&["--help".to_string()]).unwrap(), Parsed::Help);
    }

    #[test]
    fn what_is_not_listed_is_an_error_that_repeats_no_value() {
        for (args, wanted) in [
            (&["-k", "https://a.test/"][..], "-k"),
            (&["--insecure", "https://a.test/"][..], "--insecure"),
            (&["-fk", "https://a.test/"][..], "-k"),
            (&["-d", "x", "https://a.test/"][..], "-d"),
            (&["-X", "POST", "https://a.test/"][..], "-X"),
            (&["--user=me:s3cret", "https://a.test/"][..], "--user"),
            (&["-us3cret", "https://a.test/"][..], "-u"),
            (&["--output=f", "https://a.test/"][..], "next argument"),
            (&["--proto=https", "https://a.test/"][..], "next argument"),
        ] {
            let error = fetching(args).unwrap_err();
            assert!(error.contains(wanted), "{args:?}: {error}");
            assert!(!error.contains("s3cret"), "{error}");
        }
        for args in [
            &["--proto", "-all,https", "https://a.test/"][..],
            &["https://a.test/", "https://b.test/"][..],
            &["-o", "a", "-O", "https://a.test/"][..],
            &["-m", "0", "https://a.test/"][..],
            &["-m", "soon", "https://a.test/"][..],
            &["-m", "NaN", "https://a.test/"][..],
            &["-m", "1e19", "https://a.test/"][..],
            &["--connect-timeout", "1e19", "https://a.test/"][..],
            &["-A", "a\nb", "https://a.test/"][..],
            &["--connect-timeout", "-1", "https://a.test/"][..],
            &["-H", "no colon", "https://a.test/"][..],
            &["-H", "Removed:", "https://a.test/"][..],
            &["-H", "bad name: x", "https://a.test/"][..],
            &["-H", "A: b\r\nC: d", "https://a.test/"][..],
            &["-o"][..],
            &["-s"][..],
        ] {
            assert!(fetching(args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn every_listed_option_is_in_the_help_and_the_version_says_what_this_is() {
        assert!(version().starts_with("td-curl "));
        assert!(version().contains("not curl"));
        assert!(HELP.contains("It is not curl"));
        for option in [
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--output",
            "--remote-name",
            "--header",
            "--user-agent",
            "--proto =https",
            "--tlsv1.2",
            "--connect-timeout",
            "--max-time",
            "--cacert",
            "--version",
            "--help",
        ] {
            assert!(HELP.contains(option), "{option}");
        }
    }

    #[test]
    fn urls_are_held_to_what_this_does() {
        let url = Url::parse("https://a.test:8443/d/f.tar?x=1#frag").unwrap();
        assert_eq!(url.text, "https://a.test:8443/d/f.tar?x=1");
        assert_eq!(url.origin, "https://a.test:8443");
        assert_eq!(url.host, "a.test");
        assert_eq!(url.remote_name().unwrap(), "f.tar");
        // A bare host still has a path for a redirect to resolve from.
        assert_eq!(
            Url::parse("https://a.test").unwrap().text,
            "https://a.test/"
        );
        assert_eq!(
            Url::parse("https://a.test?x=1").unwrap().text,
            "https://a.test/?x=1"
        );
        assert_eq!(
            Url::parse("https://a.test")
                .unwrap()
                .redirected("login.php")
                .unwrap()
                .text,
            "https://a.test/login.php"
        );
        assert!(Url::parse("https://a.test/-")
            .unwrap()
            .remote_name()
            .is_err());
        for name in [
            "https://a.test",
            "https://a.test/",
            "https://a.test/d/",
            "https://a.test/..",
        ] {
            assert!(Url::parse(name).unwrap().remote_name().is_err(), "{name}");
        }
        for refused in [
            "ftp://a.test/",
            "file:///etc/passwd",
            "a.test/f",
            "https://me:s3cret@a.test/",
            "https://token@a.test/",
        ] {
            let error = Url::parse(refused).unwrap_err();
            assert!(!error.contains("s3cret"), "{error}");
        }
    }

    #[test]
    fn redirects_may_not_downgrade_and_may_resolve() {
        let url = Url::parse("https://a.test/d/f?x=1").unwrap();
        for (location, wanted) in [
            ("g", "https://a.test/d/g"),
            ("?y=2", "https://a.test/d/f?y=2"),
            ("/g", "https://a.test/g"),
            ("//b.test/g", "https://b.test/g"),
            ("https://b.test/g?y=2", "https://b.test/g?y=2"),
        ] {
            assert_eq!(url.redirected(location).unwrap().text, wanted, "{location}");
        }
        for refused in [
            "http://a.test/g",
            "ftp://a.test/g",
            "https://me:pw@a.test/g",
            "../g",
        ] {
            assert!(url.redirected(refused).is_err(), "{refused}");
        }
        let plain = Url::parse("http://a.test/f").unwrap();
        assert!(plain.redirected("https://a.test/f").unwrap().https);
    }

    #[test]
    fn the_bundle_and_the_environment_choose_in_order() {
        let none = |_: &str| None;
        let both = |name: &str| match name {
            "CURL_CA_BUNDLE" => Some("/curl".to_string()),
            "SSL_CERT_FILE" => Some("/ssl".to_string()),
            _ => None,
        };
        let cert = |name: &str| (name == "SSL_CERT_FILE").then(|| "/ssl".to_string());
        let mut options = Options::default();
        assert_eq!(named_bundle(&options, &none), None);
        assert_eq!(named_bundle(&options, &cert).unwrap(), "/ssl");
        assert_eq!(named_bundle(&options, &both).unwrap(), "/curl");
        options.cacert = Some("/flag".to_string());
        assert_eq!(named_bundle(&options, &both).unwrap(), "/flag");

        let options = fetching(&["https://a.test/"]).unwrap();
        let dir = |name: &str| (name == "SSL_CERT_DIR").then(|| "/d".to_string());
        let error = fetch(&options, &dir).unwrap_err();
        assert!(error.message.contains("SSL_CERT_DIR"), "{error:?}");

        // A named file settles it; the directory only refuses what it would change.
        let named = fetching(&["--cacert", "/no/such", "ftp://a.test/"]).unwrap();
        let error = fetch(&named, &dir).unwrap_err();
        assert!(!error.message.contains("SSL_CERT_DIR"), "{error:?}");

        let options = fetching(&["--proto", "=https", "http://a.test/"]).unwrap();
        let error = fetch(&options, &none).unwrap_err();
        assert!(error.message.contains("--proto"), "{error:?}");
    }

    #[test]
    fn binary_output_stops_short_of_a_terminal_and_only_there() {
        let mut out = Vec::new();
        let error = copy(&mut &b"a\0b"[..], &mut out, "x", true).unwrap_err();
        assert_eq!(error.code, 23);
        assert!(out.is_empty());
        copy(&mut &b"a\0b"[..], &mut out, "x", false).unwrap();
        assert_eq!(out, b"a\0b");
        out.clear();
        copy(&mut &b"text\n"[..], &mut out, "x", true).unwrap();
        assert_eq!(out, b"text\n");
        // A NUL past the first read is still binary.
        out.clear();
        let mut later = (&b"text\n"[..]).chain(&b"a\0b"[..]);
        assert!(copy(&mut later, &mut out, "x", true).is_err());
        assert_eq!(out, b"text\n");
    }
}
