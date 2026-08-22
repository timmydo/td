// http — the one HTTP GET the three applets share, with the timeouts ureq does not
// default to.
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
use std::time::{Duration, Instant};

/// Long enough that a saturated link mid-tarball never trips it, short enough that a
/// silent peer is noticed while an operator is still watching.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// The connect half of ureq's default, restated here so both halves are visible at
/// one place rather than one being a default and the other a setting.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

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

fn agent_before(deadline: Instant) -> Result<ureq::Agent, String> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| "HTTP request exceeded its absolute deadline".to_string())?;
    Ok(ureq::AgentBuilder::new()
        // ureq's global timeout covers connect, redirects, and a trickling
        // response body. DNS uses the platform's blocking resolver and cannot
        // be interrupted, but is checked immediately when it returns.
        .timeout(remaining)
        .timeout_connect(CONNECT_TIMEOUT.min(remaining))
        .build())
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
) -> Result<(), Attempt> {
    let resp = match agent.get(url).call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if retryable_status(code) => {
            return Err(Attempt::Transport(format!("{url}: status {code}")))
        }
        Err(e @ ureq::Error::Status(..)) => return Err(Attempt::Answered(e.to_string())),
        Err(e) => return Err(Attempt::Transport(e.to_string())),
    };
    let mut output = File::create(path).map_err(|e| {
        Attempt::Answered(format!("create streamed response {}: {e}", path.display()))
    })?;
    let mut reader = resp.into_reader();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| Attempt::Transport(format!("read {url}: {e}")))?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
        if total > max_bytes {
            return Err(Attempt::Answered(format!(
                "{url}: response exceeds its {max_bytes}-byte limit"
            )));
        }
        let chunk = buf.get(..n).ok_or_else(|| {
            Attempt::Answered(format!("{url}: response read exceeded its buffer"))
        })?;
        output.write_all(chunk).map_err(|e| {
            Attempt::Answered(format!("write streamed response {}: {e}", path.display()))
        })?;
    }
    output.flush().map_err(|e| {
        Attempt::Answered(format!("flush streamed response {}: {e}", path.display()))
    })
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
            Some(deadline) => agent_before(deadline)?,
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
    get_to_file_with_deadline(url, path, max_bytes, None)
}

pub(crate) fn get_to_file_before(
    url: &str,
    path: &Path,
    max_bytes: u64,
    deadline: Instant,
) -> Result<(), String> {
    get_to_file_with_deadline(url, path, max_bytes, Some(deadline))
}

fn get_to_file_with_deadline(
    url: &str,
    path: &Path,
    max_bytes: u64,
    deadline: Option<Instant>,
) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        let request_agent = match deadline {
            Some(deadline) => agent_before(deadline)?,
            None => agent(),
        };
        match get_to_file_once(&request_agent, url, path, max_bytes) {
            Ok(()) => return Ok(()),
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
        let path = std::env::temp_dir().join(format!(
            "td-http-stream-limit-{}",
            std::process::id()
        ));
        let err = get_to_file_once(&agent(), &format!("http://127.0.0.1:{port}/x"), &path, 4)
            .expect_err("a response above the ceiling must fail");
        assert!(
            matches!(&err, Attempt::Answered(message) if message.contains("exceeds")),
            "unexpected streamed-limit error"
        );
        let _ = std::fs::remove_file(path);
        let _ = server.join();
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
        let err = get_body(&format!("http://127.0.0.1:{port}/missing"))
            .expect_err("404 is not a body");
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
}
