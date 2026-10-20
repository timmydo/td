// http — the one HTTP GET the three applets share, with the timeouts ureq does not
// default to.
//
// A pinned blob on an allowlisted container registry (`TOKEN_REGISTRIES`) is the
// one URL shape that needs more than a GET: it takes an anonymous pull token, a
// bearer GET with redirects off, then the registry's redirect target without the
// token. Callers see an ordinary `get_to_file`; the sha256 pin still decides.
//
// ureq 2's own defaults are `timeout_connect: Some(30s)` and `timeout_read: None`
// (agent.rs:256). The connect half is already right, and ureq walks every resolved
// address when one refuses (stream.rs `connect_host`), so a host whose first address
// is unreachable still reaches a working one. The READ half is the gap: a peer that
// completes the handshake, sends part of a body and then goes silent leaves a socket
// that is neither closed nor readable, and with no read timeout the applet blocks in
// `read_to_end` forever. That is not hypothetical — a `td-feed warm sources` on this
// repo's own host took ~900 KB of a gnu.org tarball over a black-holed IPv6 path and
// then held the entire cold ladder climb for twenty minutes, printing nothing, with
// the kernel still reporting the connection ESTABLISHED.
//
// `timeout_read` is a per-read deadline (`socket.set_read_timeout`), NOT a transfer
// budget, so a multi-hundred-megabyte tarball on a slow link is unaffected: only a
// gap between two reads trips it. A stalled attempt is then retried, because the
// alternative to a hang must not be a cold climb that dies on one bad socket.
//
// What this does NOT close, stated so a reader does not trust a stronger property
// than the code delivers: a peer that TRICKLES — one byte every 59
// seconds — never trips a per-read deadline, and with no minimum-throughput floor it
// can still hold a fetch indefinitely. Nor does either deadline cover DNS: ureq 2
// resolves through a blocking `to_socket_addrs` outside both, so a wedged resolver is
// its own hang. Both are real and neither is what was observed; a throughput floor is
// a bigger change than this one, in the crate where td's only external dependencies
// live, and belongs in its own landing.
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Long enough that a saturated link mid-tarball never trips it, short enough that a
/// silent peer is noticed while an operator is still watching.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// The connect half of ureq's default, restated here so both halves are visible at
/// one place rather than one being a default and the other a setting.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Writes are small (request heads); a peer not reading them is as stuck as one not
/// writing.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Total attempts per URL. A stall costs READ_TIMEOUT before it is noticed, so this
/// is bounded low on purpose: three attempts fail in about three minutes, which reads
/// as a failure, where five would read as another hang.
const ATTEMPTS: u32 = 3;

/// Pause before trying again. Not every retryable failure is a stall — a refused
/// connection comes back at once, and three back-to-back attempts are three ways of
/// asking the same question in the same instant.
const BACKOFF: Duration = Duration::from_secs(2);

/// Bodies retained in memory are protocol metadata, errors, or selftest
/// responses. Archives and other potentially large artifacts use
/// `get_to_file`; refusing an oversized metadata response keeps an accidental
/// caller of `get_body` from recreating the old whole-archive allocation.
const MAX_IN_MEMORY_BODY_BYTES: u64 = 64 * 1024 * 1024;

/// The statuses worth asking again about: rate limiting and the two "try later"
/// gateway answers a mirror or CDN gives under load. Every other status is the
/// server's ANSWER — including 502, which td's OWN cargo proxy returns for a checksum
/// mismatch and whose selftest requires exactly once.
fn retryable_status(code: u16) -> bool {
    matches!(code, 429 | 503 | 504)
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .timeout_write(WRITE_TIMEOUT)
        .build()
}

fn agent_no_redirects() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .timeout_write(WRITE_TIMEOUT)
        .redirects(0)
        .build()
}

fn agent_before(deadline: Instant, allow_redirects: bool) -> Result<ureq::Agent, String> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| "HTTP request exceeded its absolute deadline".to_string())?;
    let mut builder = ureq::AgentBuilder::new()
        // ureq's global timeout covers connect, redirects, and a trickling
        // response body. DNS uses the platform's blocking resolver and cannot
        // be interrupted, but is checked immediately when it returns.
        .timeout(remaining)
        .timeout_connect(CONNECT_TIMEOUT.min(remaining));
    if !allow_redirects {
        builder = builder.redirects(0);
    }
    Ok(builder.build())
}

fn retry_pause(deadline: Option<Instant>) -> Result<(), String> {
    let delay = match deadline {
        Some(deadline) => deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| "HTTP request exceeded its absolute deadline".to_string())?
            .min(BACKOFF),
        None => BACKOFF,
    };
    std::thread::sleep(delay);
    Ok(())
}

/// A failed attempt, split by whether trying again could change the answer.
enum Attempt {
    /// The peer ANSWERED, with a status the caller did not want. Retrying is three
    /// times the wait for the same no — and a `no` is load-bearing here: a 404 is how
    /// a substitute miss reports itself (subst.rs narinfo), how the feed probes
    /// whether an entry is already served, and what the cargo-proxy selftest requires
    /// for a crate it deliberately asks for and must not get.
    Answered(String),
    /// Nothing answered, the answer stopped arriving mid-body, or the server said to
    /// come back later. The retryable one.
    Transport(String),
}

fn get_once(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, Attempt> {
    // ureq's Display already names the URL, so no `GET {url}:` prefix here.
    let resp = match agent.get(url).call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if retryable_status(code) => {
            return Err(Attempt::Transport(format!("{url}: status {code}")))
        }
        Err(e @ ureq::Error::Status(..)) => return Err(Attempt::Answered(e.to_string())),
        Err(e) => return Err(Attempt::Transport(e.to_string())),
    };
    let mut body = Vec::new();
    // A body that stops mid-transfer is a TRANSPORT failure however good the status
    // was — and it is the exact failure this module exists for.
    resp.into_reader()
        .take(MAX_IN_MEMORY_BODY_BYTES.saturating_add(1))
        .read_to_end(&mut body)
        .map_err(|e| Attempt::Transport(format!("read {url}: {e}")))?;
    if u64::try_from(body.len()).unwrap_or(u64::MAX) > MAX_IN_MEMORY_BODY_BYTES {
        return Err(Attempt::Answered(format!(
            "{url}: in-memory response exceeds its {MAX_IN_MEMORY_BODY_BYTES}-byte limit"
        )));
    }
    Ok(body)
}

fn get_to_file_once(
    agent: &ureq::Agent,
    url: &str,
    path: &Path,
    max_bytes: u64,
    aggregate: Option<(&AtomicU64, u64)>,
) -> Result<u64, Attempt> {
    let resp = match agent.get(url).call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if retryable_status(code) => {
            return Err(Attempt::Transport(format!("{url}: status {code}")))
        }
        Err(e @ ureq::Error::Status(..)) => return Err(Attempt::Answered(e.to_string())),
        Err(e) => return Err(Attempt::Transport(e.to_string())),
    };
    stream_response(resp, url, path, max_bytes, aggregate)
}

fn stream_response(
    resp: ureq::Response,
    url: &str,
    path: &Path,
    max_bytes: u64,
    aggregate: Option<(&AtomicU64, u64)>,
) -> Result<u64, Attempt> {
    if (300..400).contains(&resp.status()) {
        return Err(Attempt::Answered(format!(
            "{url}: redirect status {} is refused",
            resp.status()
        )));
    }
    let mut output = File::create(path).map_err(|e| {
        Attempt::Answered(format!("create streamed response {}: {e}", path.display()))
    })?;
    let mut reader = resp.into_reader();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    let mut charged = 0u64;
    let result = (|| {
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| Attempt::Transport(format!("read {url}: {e}")))?;
            if n == 0 {
                break;
            }
            let count = u64::try_from(n).map_err(|_| {
                Attempt::Answered(format!("{url}: response read count does not fit u64"))
            })?;
            total = total
                .checked_add(count)
                .ok_or_else(|| Attempt::Answered(format!("{url}: response byte count overflow")))?;
            if total > max_bytes {
                return Err(Attempt::Answered(format!(
                    "{url}: response exceeds its {max_bytes}-byte limit"
                )));
            }
            if let Some((counter, limit)) = aggregate {
                let next_charged = charged.checked_add(count).ok_or_else(|| {
                    Attempt::Answered(format!("{url}: aggregate response charge overflow"))
                })?;
                counter
                    .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                        used.checked_add(count).filter(|next| *next <= limit)
                    })
                    .map_err(|used| {
                        Attempt::Answered(format!(
                            "{url}: aggregate response exceeds its {limit}-byte limit after {used}"
                        ))
                    })?;
                charged = next_charged;
            }
            let chunk = buf.get(..n).ok_or_else(|| {
                Attempt::Answered(format!("{url}: response read exceeded its buffer"))
            })?;
            output.write_all(chunk).map_err(|e| {
                Attempt::Answered(format!("write streamed response {}: {e}", path.display()))
            })?;
        }
        output.sync_all().map_err(|e| {
            Attempt::Answered(format!("sync streamed response {}: {e}", path.display()))
        })?;
        Ok(total)
    })();
    if result.is_err() {
        if let Some((counter, _)) = aggregate {
            counter.fetch_sub(charged, Ordering::AcqRel);
        }
    }
    result
}

/// Registries whose blob URLs `get_to_file` fetches through the OCI
/// distribution token flow: (registry host, anonymous token realm, service).
/// A pin names the registry's own `/v2/<repository>/blobs/sha256:<hex>` URL,
/// never the expiring CDN URL the registry redirects to.
const TOKEN_REGISTRIES: &[(&str, &str, &str)] = &[(
    "registry-1.docker.io",
    "https://auth.docker.io/token",
    "registry.docker.io",
)];

/// Docker Hub's anonymous pull tokens are a few kilobytes of JWT, inside a
/// small JSON object; a larger answer is refused before it is buffered.
const MAX_TOKEN_BYTES: usize = 16 * 1024;
const MAX_TOKEN_RESPONSE_BYTES: u64 = 64 * 1024;

/// A blob URL on an allowlisted registry: the token URL for its pull scope,
/// and whether its redirect target must be HTTPS (always, for allowlisted
/// registries; loopback tests build one that is not).
#[derive(Debug, PartialEq)]
struct RegistryBlob {
    token_url: String,
    require_https: bool,
}

fn repository_name_is_valid(name: &str) -> bool {
    name.split('/').all(|part| {
        let bytes = part.as_bytes();
        bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.iter().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
            })
    })
}

fn registry_blob(url: &str) -> Option<RegistryBlob> {
    let (host, path) = url.strip_prefix("https://")?.split_once('/')?;
    let &(_, realm, service) = TOKEN_REGISTRIES.iter().find(|entry| entry.0 == host)?;
    let (repository, digest) = path.strip_prefix("v2/")?.rsplit_once("/blobs/")?;
    let hex = digest.strip_prefix("sha256:")?;
    let digest_ok = hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    (digest_ok && repository_name_is_valid(repository)).then(|| RegistryBlob {
        token_url: format!("{realm}?service={service}&scope=repository:{repository}:pull"),
        require_https: true,
    })
}

/// Bound one hop by what remains of the transfer's absolute deadline. ureq
/// starts a request's timeout when it is called, so a multi-hop flow on one
/// agent would otherwise give every hop the whole budget.
fn within_deadline(
    request: ureq::Request,
    deadline: Option<Instant>,
    label: &str,
) -> Result<ureq::Request, Attempt> {
    let Some(deadline) = deadline else {
        return Ok(request);
    };
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .map(|remaining| request.timeout(remaining))
        .ok_or_else(|| {
            Attempt::Answered(format!(
                "{label}: HTTP request exceeded its absolute deadline"
            ))
        })
}

/// Issue one registry-flow request. A URL, header or scheme ureq cannot use is
/// an answer: retrying cannot change it. `redact` keeps ureq's rendering of
/// the URL out of a failure, for the CDN hop whose URL carries an expiring
/// signature; the other hops keep ureq's full detail.
fn call_labeled(
    request: ureq::Request,
    label: &str,
    redact: bool,
) -> Result<ureq::Response, Attempt> {
    match request.call() {
        Ok(resp) => Ok(resp),
        Err(ureq::Error::Status(code, _)) if retryable_status(code) => {
            Err(Attempt::Transport(format!("{label}: status {code}")))
        }
        Err(ureq::Error::Status(code, _)) => {
            Err(Attempt::Answered(format!("{label}: status {code}")))
        }
        Err(ureq::Error::Transport(e)) => {
            let detail = if redact {
                format!("{label}: {}", e.kind())
            } else {
                e.to_string()
            };
            match e.kind() {
                ureq::ErrorKind::InvalidUrl
                | ureq::ErrorKind::UnknownScheme
                | ureq::ErrorKind::BadHeader => Err(Attempt::Answered(detail)),
                _ => Err(Attempt::Transport(detail)),
            }
        }
    }
}

fn registry_token(
    agent: &ureq::Agent,
    token_url: &str,
    deadline: Option<Instant>,
) -> Result<String, Attempt> {
    let resp = call_labeled(
        within_deadline(agent.get(token_url), deadline, token_url)?,
        token_url,
        false,
    )?;
    let answered = |what: &str| Attempt::Answered(format!("{token_url}: {what}"));
    if resp.status() != 200 {
        return Err(answered(&format!(
            "token endpoint answered status {}",
            resp.status()
        )));
    }
    let mut body = Vec::new();
    resp.into_reader()
        .take(MAX_TOKEN_RESPONSE_BYTES.saturating_add(1))
        .read_to_end(&mut body)
        .map_err(|e| Attempt::Transport(format!("read {token_url}: {e}")))?;
    if u64::try_from(body.len()).unwrap_or(u64::MAX) > MAX_TOKEN_RESPONSE_BYTES {
        return Err(answered("token response exceeds its byte limit"));
    }
    let text = std::str::from_utf8(&body).map_err(|_| answered("token response is not UTF-8"))?;
    let json = td_engine::json::parse(text)
        .map_err(|e| answered(&format!("token response is not JSON: {e}")))?;
    let token = json
        .get("token")
        .and_then(td_engine::json::Json::as_str)
        .or_else(|| {
            json.get("access_token")
                .and_then(td_engine::json::Json::as_str)
        })
        .ok_or_else(|| answered("token response names no token"))?;
    // The token becomes a header value. ureq refuses CR/LF itself, but its
    // error would print the whole Authorization header; holding the token to
    // the RFC 6750 b64token alphabet refuses it here without echoing it.
    let well_formed = !token.is_empty()
        && token.len() <= MAX_TOKEN_BYTES
        && token.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+' | b'/' | b'=')
        });
    if !well_formed {
        return Err(answered("token is empty, oversized or not a bearer token"));
    }
    Ok(token.to_string())
}

/// Fetch one registry blob: an anonymous pull token, the bearer GET with
/// redirects disabled, then the registry's redirect target WITHOUT the bearer
/// header. `agent` must not follow redirects, so every hop refuses further
/// ones and the token reaches only the allowlisted realm's registry.
fn registry_blob_to_file_once(
    agent: &ureq::Agent,
    url: &str,
    blob: &RegistryBlob,
    deadline: Option<Instant>,
    path: &Path,
    max_bytes: u64,
    aggregate: Option<(&AtomicU64, u64)>,
) -> Result<u64, Attempt> {
    let token = registry_token(agent, &blob.token_url, deadline)?;
    let request = agent
        .get(url)
        .set("Authorization", &format!("Bearer {token}"));
    let resp = call_labeled(within_deadline(request, deadline, url)?, url, false)?;
    if !(300..400).contains(&resp.status()) {
        return stream_response(resp, url, path, max_bytes, aggregate);
    }
    let location = resp
        .header("location")
        .ok_or_else(|| Attempt::Answered(format!("{url}: blob redirect names no Location")))?;
    let absolute = location.starts_with("https://")
        || (!blob.require_https && location.starts_with("http://"));
    if !absolute {
        return Err(Attempt::Answered(format!(
            "{url}: blob redirect is not an absolute HTTPS URL; refused"
        )));
    }
    let label = format!("{url} via its blob redirect");
    let request = within_deadline(agent.get(location), deadline, &label)?;
    let resp = call_labeled(request, &label, true)?;
    stream_response(resp, &label, path, max_bytes, aggregate)
}

/// The registry flow follows its one redirect itself, without the token, so
/// its agent never follows one.
fn follows_redirects(allow_redirects: bool, blob: Option<&RegistryBlob>) -> bool {
    allow_redirects && blob.is_none()
}

/// GET `url`'s body. Each attempt is a fresh `call()`, so it re-resolves and
/// re-connects rather than retrying into the socket that just failed. That is worth
/// little against a black-holed address — the resolver usually hands back the same
/// ordered list, so the likely outcome there is the same dead address again — and
/// worth a lot against a mirror having a moment. Every failed attempt is announced: a
/// retry that printed nothing would restore the very silence the read timeout exists
/// to break.
pub(crate) fn get_body(url: &str) -> Result<Vec<u8>, String> {
    get_body_with_deadline(url, None)
}

pub(crate) fn get_body_before(url: &str, deadline: Instant) -> Result<Vec<u8>, String> {
    get_body_with_deadline(url, Some(deadline))
}

fn get_body_with_deadline(url: &str, deadline: Option<Instant>) -> Result<Vec<u8>, String> {
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        // A FRESH agent per attempt. A shared one pools connections by host, and
        // while ureq does not recycle an errored stream today, that would make the
        // re-resolve/re-connect above an accident rather than a property — and the
        // whole reason to try again is to reach somewhere else.
        let request_agent = match deadline {
            Some(deadline) => agent_before(deadline, true)?,
            None => agent(),
        };
        match get_once(&request_agent, url) {
            Ok(body) => return Ok(body),
            Err(Attempt::Answered(e)) => return Err(e),
            Err(Attempt::Transport(e)) => {
                last = e;
                if attempt < ATTEMPTS {
                    eprintln!("td-net: {last} (attempt {attempt}/{ATTEMPTS}) — retrying");
                    retry_pause(deadline)?;
                }
            }
        }
    }
    Err(format!("{last} (after {ATTEMPTS} attempts)"))
}

/// GET a potentially large body directly into `path`, retrying transport
/// failures from a freshly truncated file and refusing a response above the
/// caller's declared ceiling. This is the cold-source path; unlike `get_body`,
/// memory use stays constant as an archive grows.
pub(crate) fn get_to_file(url: &str, path: &Path, max_bytes: u64) -> Result<(), String> {
    get_to_file_with_deadline(url, path, max_bytes, None, None, true).map(|_| ())
}

pub(crate) fn get_to_file_before(
    url: &str,
    path: &Path,
    max_bytes: u64,
    deadline: Instant,
) -> Result<(), String> {
    get_to_file_with_deadline(url, path, max_bytes, Some(deadline), None, true).map(|_| ())
}

/// Stream one immutable object under both byte budgets and an absolute graph
/// deadline. Failed attempts roll their reservation back before retrying, and
/// redirect refusal remains active on the deadline-built agent.
pub(crate) fn get_to_file_accounted_no_redirects_before(
    url: &str,
    path: &Path,
    max_bytes: u64,
    aggregate: &AtomicU64,
    max_aggregate: u64,
    deadline: Instant,
) -> Result<u64, String> {
    get_to_file_with_deadline(
        url,
        path,
        max_bytes,
        Some(deadline),
        Some((aggregate, max_aggregate)),
        false,
    )
}

fn get_to_file_with_deadline(
    url: &str,
    path: &Path,
    max_bytes: u64,
    deadline: Option<Instant>,
    aggregate: Option<(&AtomicU64, u64)>,
    allow_redirects: bool,
) -> Result<u64, String> {
    get_to_file_with_policy(
        url,
        registry_blob(url),
        path,
        max_bytes,
        deadline,
        aggregate,
        allow_redirects,
    )
}

fn get_to_file_with_policy(
    url: &str,
    blob: Option<RegistryBlob>,
    path: &Path,
    max_bytes: u64,
    deadline: Option<Instant>,
    aggregate: Option<(&AtomicU64, u64)>,
    allow_redirects: bool,
) -> Result<u64, String> {
    if blob.is_some() && !allow_redirects {
        return Err(format!(
            "{url}: a registry blob is served through a redirect, which this transfer refuses"
        ));
    }
    let follow = follows_redirects(allow_redirects, blob.as_ref());
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        let request_agent = match deadline {
            Some(deadline) => agent_before(deadline, follow)?,
            None if follow => agent(),
            None => agent_no_redirects(),
        };
        let result = match &blob {
            Some(blob) => registry_blob_to_file_once(
                &request_agent,
                url,
                blob,
                deadline,
                path,
                max_bytes,
                aggregate,
            ),
            None => get_to_file_once(&request_agent, url, path, max_bytes, aggregate),
        };
        match result {
            Ok(bytes) => return Ok(bytes),
            Err(Attempt::Answered(e)) => return Err(e),
            Err(Attempt::Transport(e)) => {
                last = e;
                if attempt < ATTEMPTS {
                    eprintln!("td-net: {last} (attempt {attempt}/{ATTEMPTS}) — retrying");
                    retry_pause(deadline)?;
                }
            }
        }
    }
    Err(format!("{last} (after {ATTEMPTS} attempts)"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    /// The whole point of the read timeout: a peer that completes the handshake,
    /// promises a body and then never sends it must END the request rather than
    /// block forever. Served on loopback so the test needs no network.
    ///
    /// The agent under test is built here with a short read timeout rather than
    /// reusing `agent()` — a test that waited out the real 60s would be the hang it
    /// is checking for — so what this pins is that a read timeout is CONSULTED at
    /// all, i.e. that `into_reader()` respects the agent's socket deadline.
    #[test]
    fn a_body_that_never_arrives_ends_the_request() {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l,
            // A sandbox with no loopback bind cannot run this; it is not a failure
            // of the code under test.
            Err(_) => return,
        };
        let port = match listener.local_addr() {
            Ok(a) => a.port(),
            Err(_) => return,
        };
        let server = std::thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let mut scratch = [0u8; 1024];
                let _ = conn.read(&mut scratch);
                // A complete head promising ten bytes, then silence — the shape of
                // the stall, minus the twenty-minute wait.
                let _ = conn.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n",
                );
                let _ = conn.flush();
                // Outlive the client's 300ms deadline without costing the suite seconds.
                std::thread::sleep(Duration::from_millis(800));
            }
        });
        let stalling = ureq::AgentBuilder::new()
            .timeout_read(Duration::from_millis(300))
            .build();
        let err = get_once(&stalling, &format!("http://127.0.0.1:{port}/x"))
            .err()
            .expect("a body that never arrives must not read as success");
        // A stall mid-body is TRANSPORT, not an answer — the status was 200.
        assert!(
            matches!(&err, Attempt::Transport(m) if m.contains("read ")),
            "{}",
            match &err {
                Attempt::Transport(m) | Attempt::Answered(m) => m,
            }
        );
        let _ = server.join();
    }

    #[test]
    fn an_absolute_deadline_stops_a_trickling_body() {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(_) => return,
        };
        let port = match listener.local_addr() {
            Ok(addr) => addr.port(),
            Err(_) => return,
        };
        let server = std::thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let mut scratch = [0u8; 1024];
                let _ = conn.read(&mut scratch);
                let _ = conn.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\n",
                );
                for byte in b"abcdefghijklmnopqrst" {
                    if conn.write_all(&[*byte]).is_err() {
                        break;
                    }
                    let _ = conn.flush();
                    std::thread::sleep(Duration::from_millis(75));
                }
            }
        });
        let started = Instant::now();
        let deadline = started + Duration::from_millis(250);

        let error = get_body_before(&format!("http://127.0.0.1:{port}/x"), deadline)
            .expect_err("a trickling body must not reset the absolute deadline");

        assert!(
            error.contains("deadline") || error.contains("timed out"),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = server.join();
    }

    /// The test above proves ureq HONOURS a read timeout; this proves the applets'
    /// own agent carries one. Both halves are needed — ureq's default `timeout_read`
    /// is None (agent.rs:257), and that default is the hang, so the assertion on the
    /// default is what keeps this test meaningful rather than vacuous.
    #[test]
    fn the_shared_agent_sets_a_read_timeout() {
        let ours = format!("{:?}", agent());
        assert!(ours.contains("timeout_read: Some"), "{ours}");
        let default = format!("{:?}", ureq::AgentBuilder::new().build());
        assert!(default.contains("timeout_read: None"), "{default}");
    }

    #[test]
    fn a_streamed_body_stops_at_its_declared_ceiling() {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(_) => return,
        };
        let port = match listener.local_addr() {
            Ok(addr) => addr.port(),
            Err(_) => return,
        };
        let server = std::thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let mut scratch = [0u8; 1024];
                let _ = conn.read(&mut scratch);
                let _ = conn.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n12345",
                );
            }
        });
        let path =
            std::env::temp_dir().join(format!("td-http-stream-limit-{}", std::process::id()));
        let err = get_to_file_once(
            &agent(),
            &format!("http://127.0.0.1:{port}/x"),
            &path,
            4,
            None,
        )
        .expect_err("a response above the ceiling must fail");
        assert!(
            matches!(&err, Attempt::Answered(message) if message.contains("exceeds")),
            "unexpected streamed-limit error"
        );
        let _ = std::fs::remove_file(path);
        let _ = server.join();
    }

    #[test]
    fn a_streamed_body_cannot_overshoot_a_shared_aggregate_ceiling() {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(_) => return,
        };
        let port = match listener.local_addr() {
            Ok(addr) => addr.port(),
            Err(_) => return,
        };
        let server = std::thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let mut scratch = [0u8; 1024];
                let _ = conn.read(&mut scratch);
                let body = vec![b'x'; 70_000];
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = conn.write_all(head.as_bytes());
                let _ = conn.write_all(&body);
            }
        });
        let path =
            std::env::temp_dir().join(format!("td-http-aggregate-limit-{}", std::process::id()));
        let aggregate = AtomicU64::new(3);

        let error = get_to_file_accounted_no_redirects_before(
            &format!("http://127.0.0.1:{port}/x"),
            &path,
            100_000,
            &aggregate,
            66_000,
            Instant::now() + Duration::from_secs(2),
        )
        .expect_err("a response crossing the aggregate ceiling must fail");

        assert!(error.contains("aggregate response exceeds"), "{error}");
        assert_eq!(aggregate.load(Ordering::Acquire), 3);
        assert!(std::fs::metadata(&path).unwrap().len() > 0);
        let _ = std::fs::remove_file(path);
        let _ = server.join();
    }

    #[test]
    fn immutable_object_deadlines_do_not_reenable_redirects() {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(_) => return,
        };
        let port = match listener.local_addr() {
            Ok(addr) => addr.port(),
            Err(_) => return,
        };
        if listener.set_nonblocking(true).is_err() {
            return;
        }
        let requests = std::sync::Arc::new(AtomicU64::new(0));
        let observed = std::sync::Arc::clone(&requests);
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut connection, _)) => {
                        observed.fetch_add(1, Ordering::AcqRel);
                        let mut request = [0u8; 1024];
                        let _ = connection.read(&mut request);
                        let answer = format!(
                            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/second\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                        let _ = connection.write_all(answer.as_bytes());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::yield_now();
                    }
                    Err(_) => return,
                }
            }
        });
        let path = std::env::temp_dir().join(format!("td-http-no-redirect-{}", std::process::id()));
        let aggregate = AtomicU64::new(0);
        let error = get_to_file_with_deadline(
            &format!("http://127.0.0.1:{port}/first"),
            &path,
            1024,
            Some(Instant::now() + Duration::from_secs(2)),
            Some((&aggregate, 1024)),
            false,
        )
        .expect_err("an immutable object redirect must be an answer, not navigation");

        assert!(error.contains("302"), "{error}");
        let _ = server.join();
        assert_eq!(requests.load(Ordering::Acquire), 1);
        let _ = std::fs::remove_file(path);
    }

    /// The split is by CODE, not by error variant: a mirror answering 503 under load
    /// during a cold warm is the transient this module exists to survive, and failing
    /// the whole entry on the first one would be the shape of failure it set out to
    /// remove. 502 is deliberately NOT here — td's own cargo proxy returns it for a
    /// checksum mismatch, and a selftest requires that answer exactly once.
    #[test]
    fn only_come_back_later_statuses_are_retried() {
        for code in [429, 503, 504] {
            assert!(retryable_status(code), "{code} should be retried");
        }
        for code in [400, 401, 403, 404, 410, 500, 501, 502] {
            assert!(!retryable_status(code), "{code} is an answer");
        }
    }

    /// A status is an ANSWER, and three of this crate's call sites depend on getting
    /// it once: a substitute miss (404), the feed's already-served probe, and the
    /// cargo-proxy selftest that asks for a crate it must NOT be given. Retrying
    /// those would triple every miss and make a test that requires a failure wait for
    /// three. Counted by connections accepted, because the caller cannot see attempts.
    #[test]
    fn a_status_answer_is_not_retried() {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(_) => return,
        };
        let port = match listener.local_addr() {
            Ok(a) => a.port(),
            Err(_) => return,
        };
        let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&accepted);
        let server = std::thread::spawn(move || {
            // Serve one more than a single attempt, so a retrying caller WOULD be
            // answered and the count could exceed 1 — the assertion then means
            // "did not try again", not "could not".
            for _ in 0..ATTEMPTS {
                match listener.accept() {
                    Ok((mut conn, _)) => {
                        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let mut scratch = [0u8; 1024];
                        let _ = conn.read(&mut scratch);
                        let _ = conn.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\
                              Connection: close\r\n\r\n",
                        );
                    }
                    Err(_) => return,
                }
            }
        });
        let err =
            get_body(&format!("http://127.0.0.1:{port}/missing")).expect_err("404 is not a body");
        assert!(err.contains("404"), "{err}");
        assert!(
            !err.contains("attempts"),
            "an answered request was retried: {err}"
        );
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Unblock the server's remaining accepts so the thread can finish.
        for _ in 1..ATTEMPTS {
            let _ = std::net::TcpStream::connect(("127.0.0.1", port));
        }
        let _ = server.join();
    }

    /// A retry that gave up after one attempt would turn every transient stall into a
    /// failed climb; one that never gave up would be the hang again.
    #[test]
    fn a_url_that_never_answers_gives_up_bounded() {
        // Port 0 is not connectable, so every attempt fails immediately and the loop
        // is what is being timed, not a socket.
        let err = get_body("http://127.0.0.1:0/x").expect_err("port 0 cannot serve");
        assert!(err.contains(&format!("after {ATTEMPTS} attempts")), "{err}");
    }

    const BLOB_HEX: &str = "ecc510c1e359bdc007b39802570bb7e4dec7d6ebccf357c3201a74299113e05e";

    fn attempt_message(attempt: Attempt) -> String {
        match attempt {
            Attempt::Answered(m) => format!("answered: {m}"),
            Attempt::Transport(m) => format!("transport: {m}"),
        }
    }

    fn reply(code: u16, extra: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {code} X\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Answer up to `count` connections through `route` until `wait` passes,
    /// returning each request head. A test that allows one more connection
    /// than the flow should make can see a hop that must not happen.
    fn serve_routes(
        listener: TcpListener,
        count: usize,
        wait: Duration,
        route: impl Fn(&str) -> String + Send + 'static,
    ) -> std::thread::JoinHandle<Vec<String>> {
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            let until = Instant::now() + wait;
            if listener.set_nonblocking(true).is_err() {
                return seen;
            }
            while seen.len() < count && Instant::now() < until {
                let mut conn = match listener.accept() {
                    Ok((conn, _)) => conn,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                let _ = conn.set_nonblocking(false);
                let mut buf = [0u8; 8192];
                let n = conn.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(buf.get(..n).unwrap_or_default()).to_string();
                let _ = conn.write_all(route(&request).as_bytes());
                seen.push(request);
            }
            seen
        })
    }

    fn loopback() -> Option<(TcpListener, String)> {
        let listener = TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        Some((listener, format!("http://127.0.0.1:{port}")))
    }

    fn has_bearer(request: &str, token: &str) -> bool {
        request
            .lines()
            .any(|line| line.eq_ignore_ascii_case(&format!("authorization: Bearer {token}")))
    }

    fn first_line(request: &str) -> &str {
        request.lines().next().unwrap_or_default()
    }

    fn without_authorization(request: &str) -> bool {
        !request.to_ascii_lowercase().contains("\nauthorization:")
    }

    const BLOB_PATH: &str = "/v2/library/debian/blobs/sha256:\
                             ecc510c1e359bdc007b39802570bb7e4dec7d6ebccf357c3201a74299113e05e";

    /// The fetched bytes or error, and the request heads the registry saw.
    type Fetched = (Result<Vec<u8>, String>, Vec<String>);

    /// Drive the production dispatch, `get_to_file_with_policy`, against a
    /// loopback registry that answers `route(base, request)`.
    fn fetch_blob(
        count: usize,
        wait: Duration,
        deadline: Option<Duration>,
        route: impl Fn(&str, &str) -> String + Send + 'static,
    ) -> Option<Fetched> {
        let (listener, base) = loopback()?;
        let route_base = base.clone();
        let server = serve_routes(listener, count, wait, move |request| {
            route(&route_base, request)
        });
        let dir = crate::test_temp::TempDir::new("http-registry").unwrap();
        let path = dir.join("blob");
        let blob = RegistryBlob {
            token_url: format!("{base}/token"),
            require_https: false,
        };
        let result = get_to_file_with_policy(
            &format!("{base}{BLOB_PATH}"),
            Some(blob),
            &path,
            1024,
            deadline.map(|d| Instant::now() + d),
            None,
            true,
        )
        .map(|_| std::fs::read(&path).unwrap());
        Some((result, server.join().unwrap()))
    }

    #[test]
    fn only_allowlisted_registry_blob_urls_take_the_token_flow() {
        let url = format!("https://registry-1.docker.io{BLOB_PATH}");
        assert_eq!(
            registry_blob(&url),
            Some(RegistryBlob {
                token_url: "https://auth.docker.io/token?service=registry.docker.io\
                            &scope=repository:library/debian:pull"
                    .into(),
                require_https: true,
            })
        );
        let upper = BLOB_HEX.to_ascii_uppercase();
        for other in [
            format!("http://registry-1.docker.io{BLOB_PATH}"),
            format!("https://registry.example{BLOB_PATH}"),
            format!("https://registry-1.docker.io:443{BLOB_PATH}"),
            format!("https://registry-1.docker.io/v2/library/debian/manifests/sha256:{BLOB_HEX}"),
            format!("https://registry-1.docker.io/v2/library/debian/blobs/sha256:{upper}"),
            "https://registry-1.docker.io/v2/library/debian/blobs/sha256:abc".into(),
            format!("https://registry-1.docker.io/v2/Library/debian/blobs/sha256:{BLOB_HEX}"),
            format!("https://registry-1.docker.io/v2/library//debian/blobs/sha256:{BLOB_HEX}"),
            format!("https://registry-1.docker.io/v2/../debian/blobs/sha256:{BLOB_HEX}"),
            format!("https://registry-1.docker.io/v2/library/debian/blobs/sha512:{BLOB_HEX}"),
        ] {
            assert_eq!(registry_blob(&other), None, "{other}");
        }
    }

    #[test]
    fn only_a_plain_url_lets_its_agent_follow_redirects() {
        let blob = RegistryBlob {
            token_url: "t".into(),
            require_https: true,
        };
        assert!(follows_redirects(true, None));
        assert!(!follows_redirects(false, None));
        assert!(!follows_redirects(true, Some(&blob)));
        assert!(!follows_redirects(false, Some(&blob)));
    }

    #[test]
    fn a_registry_blob_sends_its_token_to_the_registry_and_not_to_the_redirect() {
        let Some((result, seen)) = fetch_blob(4, Duration::from_secs(2), None, |base, request| {
            let line = first_line(request);
            if line.starts_with("GET /token ") {
                reply(200, "", r#"{"token":"good.token-_=","expires_in":300}"#)
            } else if line == format!("GET {BLOB_PATH} HTTP/1.1")
                && has_bearer(request, "good.token-_=")
            {
                let cdn = format!("{base}/cdn/blob?signature=expiring");
                reply(307, &format!("Location: {cdn}\r\n"), "")
            } else if line == "GET /cdn/blob?signature=expiring HTTP/1.1"
                && without_authorization(request)
            {
                reply(200, "", "layer bytes")
            } else {
                reply(400, "", "unexpected request")
            }
        }) else {
            return;
        };
        assert_eq!(result.unwrap(), b"layer bytes");
        assert_eq!(seen.len(), 3);
    }

    /// The dispatch must hand the flow an agent that does not follow
    /// redirects: then a redirect from the CDN is refused as an answer, and
    /// the error names the pinned URL rather than the signed one. An agent
    /// that followed redirects itself would make a fourth request.
    #[test]
    fn a_redirect_from_the_cdn_is_an_answer_that_hides_the_signed_url() {
        let Some((result, seen)) =
            fetch_blob(4, Duration::from_millis(700), None, |base, request| {
                let line = first_line(request);
                if line.starts_with("GET /token ") {
                    reply(200, "", r#"{"token":"t"}"#)
                } else if line.starts_with("GET /v2/") {
                    let cdn = format!("{base}/cdn/blob?signature=expiring");
                    reply(307, &format!("Location: {cdn}\r\n"), "")
                } else if line.starts_with("GET /cdn/") {
                    reply(302, &format!("Location: {base}/elsewhere\r\n"), "")
                } else {
                    reply(200, "", "followed a redirect it must refuse")
                }
            })
        else {
            return;
        };
        let error = result.expect_err("a redirect from the CDN is refused");
        assert!(error.contains("302"), "{error}");
        assert!(!error.contains("signature"), "{error}");
        assert_eq!(seen.len(), 3);
    }

    #[test]
    fn a_throttled_token_hop_is_retried_with_a_fresh_flow() {
        let tokens = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&tokens);
        let Some((result, seen)) =
            fetch_blob(4, Duration::from_secs(5), None, move |base, request| {
                let line = first_line(request);
                if line.starts_with("GET /token ") {
                    if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                        reply(429, "", "")
                    } else {
                        reply(200, "", r#"{"token":"t"}"#)
                    }
                } else if line.starts_with("GET /v2/") && has_bearer(request, "t") {
                    reply(307, &format!("Location: {base}/cdn/blob\r\n"), "")
                } else {
                    reply(200, "", "layer bytes")
                }
            })
        else {
            return;
        };
        assert_eq!(result.unwrap(), b"layer bytes");
        assert_eq!(seen.len(), 4);
        assert_eq!(tokens.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn an_unauthorized_blob_hop_is_an_answer_not_a_retry() {
        let Some((result, seen)) = fetch_blob(3, Duration::from_millis(700), None, |_, request| {
            if first_line(request).starts_with("GET /token ") {
                reply(200, "", r#"{"token":"t"}"#)
            } else {
                reply(401, "", "")
            }
        }) else {
            return;
        };
        let error = result.expect_err("401 is the registry's answer");
        assert!(error.contains("status 401"), "{error}");
        assert!(!error.contains("attempts"), "{error}");
        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn a_blob_served_directly_needs_no_redirect() {
        let Some((result, seen)) = fetch_blob(3, Duration::from_millis(700), None, |_, request| {
            if first_line(request).starts_with("GET /token ") {
                reply(200, "", r#"{"access_token":"fallback"}"#)
            } else if has_bearer(request, "fallback") {
                reply(200, "", "direct")
            } else {
                reply(400, "", "")
            }
        }) else {
            return;
        };
        assert_eq!(result.unwrap(), b"direct");
        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn a_redirect_without_a_usable_location_is_an_answer_not_a_retry() {
        for location in ["", "Location: https://[unusable\r\n"] {
            let Some((result, seen)) =
                fetch_blob(3, Duration::from_millis(700), None, move |_, request| {
                    if first_line(request).starts_with("GET /token ") {
                        reply(200, "", r#"{"token":"t"}"#)
                    } else {
                        reply(307, location, "")
                    }
                })
            else {
                return;
            };
            let error = result.expect_err("no usable redirect target");
            assert!(!error.contains("attempts"), "{location:?}: {error}");
            assert_eq!(seen.len(), 2, "{location:?}");
        }
    }

    /// ureq starts a request's timeout when it is called, so the deadline has
    /// to be re-applied per hop: here each hop takes 250 ms of a 400 ms budget,
    /// and the second must fail rather than be granted the whole budget again.
    #[test]
    fn the_absolute_deadline_spans_every_hop() {
        let started = Instant::now();
        let Some((result, _)) = fetch_blob(
            2,
            Duration::from_secs(2),
            Some(Duration::from_millis(400)),
            |_, request| {
                std::thread::sleep(Duration::from_millis(250));
                if first_line(request).starts_with("GET /token ") {
                    reply(200, "", r#"{"token":"t"}"#)
                } else {
                    reply(200, "", "late")
                }
            },
        ) else {
            return;
        };
        assert!(result.is_err(), "a flow past its deadline must fail");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_registry_redirect_that_leaves_https_is_refused_before_it_is_followed() {
        let Some((listener, base)) = loopback() else {
            return;
        };
        let target = format!("{base}/cdn/blob");
        let server = serve_routes(listener, 3, Duration::from_millis(700), move |request| {
            if request.starts_with("GET /token") {
                reply(200, "", r#"{"token":"t"}"#)
            } else {
                reply(307, &format!("Location: {target}\r\n"), "")
            }
        });
        let dir = crate::test_temp::TempDir::new("http-registry-https").unwrap();
        let blob = RegistryBlob {
            token_url: format!("{base}/token"),
            require_https: true,
        };

        let error = registry_blob_to_file_once(
            &agent_no_redirects(),
            &format!("{base}{BLOB_PATH}"),
            &blob,
            None,
            &dir.join("blob"),
            1024,
            None,
        )
        .map_err(attempt_message)
        .expect_err("an http:// redirect target must be refused");

        assert!(error.starts_with("answered:"), "{error}");
        assert!(error.contains("not an absolute HTTPS URL"), "{error}");
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[test]
    fn a_token_response_that_is_not_a_usable_token_is_an_answer() {
        let oversized = format!(r#"{{"token":"{}"}}"#, "x".repeat(70 * 1024));
        for (response, expected) in [
            (
                reply(200, "", r#"{"token":"t\r\nX-Injected: 1"}"#),
                "not a bearer token",
            ),
            (
                reply(302, "Location: http://127.0.0.1:1/\r\n", ""),
                "token endpoint answered status 302",
            ),
            (reply(200, "", &oversized), "exceeds its byte limit"),
            (reply(200, "", r#"{"token":7}"#), "names no token"),
        ] {
            let Some((listener, base)) = loopback() else {
                return;
            };
            let server = serve_routes(listener, 1, Duration::from_secs(2), move |_| {
                response.clone()
            });
            let error = registry_token(&agent_no_redirects(), &format!("{base}/token"), None)
                .map_err(attempt_message)
                .expect_err("not a usable token");
            assert!(error.starts_with("answered:"), "{error}");
            assert!(error.contains(expected), "{error}");
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[test]
    fn a_registry_blob_is_refused_where_redirects_are() {
        let dir = crate::test_temp::TempDir::new("http-registry-policy").unwrap();
        let error = get_to_file_with_deadline(
            &format!("https://registry-1.docker.io{BLOB_PATH}"),
            &dir.join("blob"),
            1024,
            Some(Instant::now() + Duration::from_secs(2)),
            None,
            false,
        )
        .expect_err("the no-redirect policy cannot fetch a registry blob");
        assert!(error.contains("refuses"), "{error}");
    }
}
