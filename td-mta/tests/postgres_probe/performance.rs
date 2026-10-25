//! On-demand measurements of the real serving processes; see CONFORMANCE.md.
//! Each scenario prints one `td-mta-perf {json}` line. Nothing here is a
//! threshold: the asserts check only that the measured work happened.
use super::{certificate, deadline, sql, Fixture, ProbeChild, PASSWORD};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

/// The cloud listener's session and per-peer limit in this fixture.
pub(super) const SMTP_STREAMS_MAX: u64 = 4;
const SMALL_BYTES: usize = 2048;
const LARGE_BYTES: usize = 256 * 1024;
const LARGE_EVERY: u64 = 10;
/// Readers and the metadata client within the default eight HTTPS connections.
const JMAP_READERS_MAX: u64 = 6;
const QUERY_PAGE: usize = 256;
const GET_BATCH: usize = 64;
/// HTTPS connections started per second, evenly spaced. The server admits
/// at most eight per peer per fixed second, counted after each handshake,
/// and every harness request is a new connection from 127.0.0.1; spacing
/// and the margin absorb handshake jitter.
const PACED_PER_SECOND: u32 = 6;

struct Knobs {
    idle_seconds: u64,
    streams: u64,
    messages: u64,
    readers: u64,
    large_bytes: usize,
}

fn knob(name: &str, default: u64, maximum: u64) -> u64 {
    match std::env::var(name) {
        Ok(value) => {
            let value = value
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("{name} is not an unsigned integer"));
            assert!(
                (1..=maximum).contains(&value),
                "{name} must be 1..={maximum}"
            );
            value
        }
        Err(_) => default,
    }
}

impl Knobs {
    fn from_env() -> Self {
        let knobs = Self {
            idle_seconds: knob("TD_MTA_PERF_IDLE_SECONDS", 30, 3600),
            streams: knob(
                "TD_MTA_PERF_SMTP_STREAMS",
                SMTP_STREAMS_MAX,
                SMTP_STREAMS_MAX,
            ),
            messages: knob("TD_MTA_PERF_SMTP_MESSAGES", 50, 1000),
            readers: knob("TD_MTA_PERF_JMAP_READERS", 2, JMAP_READERS_MAX),
            large_bytes: {
                let value = usize::try_from(knob(
                    "TD_MTA_PERF_LARGE_BYTES",
                    LARGE_BYTES as u64,
                    16 * 1024 * 1024,
                ))
                .unwrap();
                assert!(
                    value >= SMALL_BYTES * 2,
                    "TD_MTA_PERF_LARGE_BYTES must be at least 4096"
                );
                value
            },
        };
        assert!(
            knobs.fits_body_quota(td_mta::admission::DiskLimits::default().body_bytes),
            "performance workload exceeds fixture body quota; reduce message size or count"
        );
        knobs
    }
    fn fits_body_quota(&self, quota: u64) -> bool {
        // Full-line rounding and the trusted receiver's trace fit within 4 KiB.
        let bound = (|| {
            let large = self.messages / LARGE_EVERY;
            let small = self.messages.checked_sub(large)?;
            let payload = large
                .checked_mul(u64::try_from(self.large_bytes).ok()?)?
                .checked_add(small.checked_mul(SMALL_BYTES as u64)?)?;
            payload
                .checked_add(self.messages.checked_mul(4096)?)?
                .checked_mul(self.streams)
        })();
        bound.is_some_and(|bytes| bytes <= quota)
    }
}

#[test]
fn performance_workload_refuses_permanent_quota_exhaustion() {
    let quota = td_mta::admission::DiskLimits::default().body_bytes;
    let mut knobs = Knobs {
        idle_seconds: 30,
        streams: 4,
        messages: 50,
        readers: 2,
        large_bytes: LARGE_BYTES,
    };
    assert!(knobs.fits_body_quota(quota));
    knobs.messages = 1000;
    knobs.large_bytes = 16 * 1024 * 1024;
    assert!(!knobs.fits_body_quota(quota));
    knobs.large_bytes = 1024 * 1024;
    assert!(knobs.fits_body_quota(quota));
    knobs.messages = u64::MAX;
    assert!(!knobs.fits_body_quota(quota));
}

/// `VmRSS` and `VmHWM` in KiB; HWM is the kernel's peak since exec.
fn try_memory(pid: u32) -> Option<(u64, u64)> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())?
            .parse::<u64>()
            .ok()
    };
    Some((field("VmRSS:")?, field("VmHWM:")?))
}

fn memory(pid: u32) -> (u64, u64) {
    try_memory(pid).unwrap()
}

/// The soft `RLIMIT_DATA` this process passes to the services, which the
/// cargo test runner sets.
fn data_limit() -> String {
    let limits = fs::read_to_string("/proc/self/limits").unwrap();
    limits
        .lines()
        .find_map(|line| line.strip_prefix("Max data size"))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap()
        .to_owned()
}

/// Cumulative database counters. Each `sql` call is itself one psql
/// session, which reaches the counters only after its own query, so a
/// delta includes the prior snapshot.
/// Sessions count at connect, and abandoned, fatal and killed sessions at
/// exit. A backend defers transaction counts while it keeps committing
/// within a second of its last flush, then flushes once idle for ten
/// seconds: commits and rollbacks can land in the next window.
#[derive(Clone, Copy)]
struct Database {
    sessions: u64,
    abandoned: u64,
    fatal: u64,
    killed: u64,
    commits: u64,
    rollbacks: u64,
}

impl Database {
    fn read(fixture: &Fixture) -> Self {
        // An ended backend's statistics reach the shared view within a second.
        thread::sleep(Duration::from_millis(1100));
        let row = sql(
            fixture,
            "SELECT sessions,sessions_abandoned,sessions_fatal,sessions_killed,xact_commit,xact_rollback FROM pg_catalog.pg_stat_database WHERE datname='postgres'",
        );
        let values = row
            .split('|')
            .map(|value| value.parse::<u64>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 6, "{row}");
        Self {
            sessions: values[0],
            abandoned: values[1],
            fatal: values[2],
            killed: values[3],
            commits: values[4],
            rollbacks: values[5],
        }
    }

    fn since(self, before: Self, seconds: f64) -> String {
        let minute = |after: u64, before: u64| (after - before) as f64 * 60.0 / seconds;
        format!(
            "\"seconds\":{seconds:.1},\"sessions\":{},\"sessions_abandoned\":{},\"sessions_fatal\":{},\"sessions_killed\":{},\"commits\":{},\"rollbacks\":{},\"sessions_per_minute\":{:.1},\"abandoned_per_minute\":{:.1},\"commits_per_minute\":{:.1}",
            self.sessions - before.sessions,
            self.abandoned - before.abandoned,
            self.fatal - before.fatal,
            self.killed - before.killed,
            self.commits - before.commits,
            self.rollbacks - before.rollbacks,
            minute(self.sessions, before.sessions),
            minute(self.abandoned, before.abandoned),
            minute(self.commits, before.commits),
        )
    }
}

fn connected(fixture: &Fixture) -> String {
    sql(
        fixture,
        "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres'",
    )
}

fn report(scenario: &str, fields: &str) {
    println!("td-mta-perf {{\"scenario\":\"{scenario}\",{fields}}}");
}

struct Service {
    child: ProbeChild,
    name: &'static str,
}

impl Service {
    fn start(local: &td_fs::TempDir, connection: &std::path::Path, name: &'static str) -> Self {
        let runtime = local.join(format!("{name}-runtime"));
        let scratch = local.join(format!("{name}-scratch"));
        for path in [&runtime, &scratch] {
            fs::create_dir_all(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let output = local.join(format!("{name}-output"));
        let started = Instant::now();
        let mut child = ProbeChild(
            Command::new(env!("CARGO_BIN_EXE_td-mta"))
                .args(["serve", "--connection-file"])
                .arg(connection)
                .args(["--profile", name, "--runtime"])
                .arg(&runtime)
                .arg("--scratch")
                .arg(&scratch)
                .stdout(fs::File::create(&output).unwrap())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let until = started + Duration::from_secs(60);
        loop {
            let text = fs::read_to_string(&output).unwrap();
            if text.contains("\"status\":\"ready\"") {
                break;
            }
            if let Some(status) = child.0.try_wait().unwrap() {
                panic!("{name} exited during startup ({status}): {text}");
            }
            assert!(Instant::now() < until, "{name} did not start: {text}");
            thread::sleep(Duration::from_millis(5));
        }
        let ready = started.elapsed().as_secs_f64() * 1000.0;
        let service = Self { child, name };
        let (rss, hwm) = memory(service.pid());
        report(
            "startup",
            &format!("\"profile\":\"{name}\",\"ready_ms\":{ready:.1},\"rss_kib\":{rss},\"hwm_kib\":{hwm}"),
        );
        service
    }

    fn pid(&self) -> u32 {
        self.child.0.id()
    }

    fn memory_fields(&self) -> String {
        let (rss, hwm) = memory(self.pid());
        format!("\"{0}_rss_kib\":{rss},\"{0}_hwm_kib\":{hwm}", self.name)
    }
}

fn idle(fixture: &Fixture, scenario: &str, services: &[&Service], seconds: u64) {
    let before = Database::read(fixture);
    let started = Instant::now();
    let mut held = Vec::new();
    while started.elapsed() < Duration::from_secs(seconds) {
        held.push(connected(fixture));
        thread::sleep(Duration::from_secs(5).min(Duration::from_secs(seconds)));
    }
    let elapsed = started.elapsed().as_secs_f64();
    let after = Database::read(fixture);
    let memory = services
        .iter()
        .map(|service| service.memory_fields())
        .collect::<Vec<_>>()
        .join(",");
    report(
        scenario,
        &format!(
            "{},\"connected_samples\":[{}],{memory}",
            after.since(before, elapsed),
            held.join(",")
        ),
    );
}

/// A deterministic message: every tenth is large, the rest small, with
/// 76-character lines that never begin with a dot.
fn message(stream: u64, index: u64, large_bytes: usize) -> Vec<u8> {
    let size = if index % LARGE_EVERY == LARGE_EVERY - 1 {
        large_bytes
    } else {
        SMALL_BYTES
    };
    let mut bytes = format!(
        "From: sender@remote.test\r\nTo: main@example.test\r\nSubject: performance {stream}.{index}\r\nMessage-ID: <perf-{stream}-{index}@remote.test>\r\nDate: Sat, 24 Oct 2026 00:00:00 +0000\r\n\r\n"
    )
    .into_bytes();
    let mut seed = stream.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ index;
    while bytes.len() < size {
        for _ in 0..76 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            bytes.push(b'a' + (seed % 26) as u8);
        }
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(b".\r\n");
    bytes
}

const RETRY_PAUSE: Duration = Duration::from_millis(50);
const ATTEMPTS_MAX: u64 = 200;
const HTTP_ATTEMPTS_MAX: u64 = 1000;
/// Beyond the server's ten-minute final-reply bound, so a slow commit is
/// waited for rather than retried as a duplicate.
const SMTP_REPLY_TIMEOUT: Duration = Duration::from_secs(660);

/// A reply's code, or `None` once the server has closed the connection.
type Code = Option<u16>;

enum Attempt {
    /// Milliseconds from the final dot to the 250 reply.
    Accepted(f64),
    /// A 4xx other than 421 at the named phase, after this many milliseconds.
    Deferred(&'static str, f64),
    /// The server closed, or announced closing with 421, before the dot.
    Closed,
    /// The same after the dot: an uncertain publication closes without a
    /// reply, so the retried message may be stored twice.
    ClosedAfterDot,
}

struct Stream {
    reader: BufReader<TcpStream>,
}

impl Stream {
    /// Retries a 421 greeting, which a session slot not yet released by a
    /// just-closed connection can cause; returns the session and refusals.
    fn connect(address: SocketAddr) -> (Self, u64) {
        for refused in 0..ATTEMPTS_MAX {
            let socket = TcpStream::connect(address).unwrap();
            socket.set_read_timeout(Some(SMTP_REPLY_TIMEOUT)).unwrap();
            let mut stream = Self {
                reader: BufReader::new(socket),
            };
            match stream.reply() {
                Some(220) => {
                    assert_eq!(stream.command(b"EHLO perf.remote.test\r\n"), Some(250));
                    return (stream, refused);
                }
                Some(421) | None => thread::sleep(RETRY_PAUSE),
                Some(code) => panic!("greeting: unexpected {code}"),
            }
        }
        panic!("SMTP sessions were never admitted");
    }

    fn reply(&mut self) -> Code {
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => return None,
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    panic!("SMTP reply timed out")
                }
                Err(_) => return None,
            }
            if line.as_bytes().get(3) == Some(&b' ') {
                return Some(line.get(..3)?.parse().unwrap());
            }
        }
    }

    fn command(&mut self, bytes: &[u8]) -> Code {
        self.reader.get_mut().write_all(bytes).ok()?;
        self.reply()
    }

    fn attempt(&mut self, body: &[u8]) -> Attempt {
        let (head, dot) = body.split_at(body.len() - 3);
        for (phase, command, expected) in [
            (
                "mail",
                b"MAIL FROM:<sender@remote.test>\r\n".as_slice(),
                250,
            ),
            ("rcpt", b"RCPT TO:<main@example.test>\r\n".as_slice(), 250),
            ("data", b"DATA\r\n".as_slice(), 354),
        ] {
            let started = Instant::now();
            match self.command(command) {
                Some(code) if code == expected => {}
                Some(421) | None => return Attempt::Closed,
                Some(400..=499) => {
                    return Attempt::Deferred(phase, started.elapsed().as_secs_f64() * 1000.0)
                }
                Some(code) => panic!("{phase}: unexpected {code}"),
            }
        }
        if self.reader.get_mut().write_all(head).is_err() {
            return Attempt::Closed;
        }
        let started = Instant::now();
        let elapsed = || started.elapsed().as_secs_f64() * 1000.0;
        match self.command(dot) {
            Some(250) => Attempt::Accepted(elapsed()),
            Some(421) | None => Attempt::ClosedAfterDot,
            Some(400..=499) => Attempt::Deferred("dot", elapsed()),
            Some(code) => panic!("dot: unexpected {code}"),
        }
    }
}

const PHASES: &[&str] = &["mail", "rcpt", "data", "dot"];

#[derive(Default)]
struct Intake {
    accepted_small: Vec<f64>,
    accepted_large: Vec<f64>,
    to_accept: Vec<f64>,
    deferral_waits: Vec<f64>,
    deferrals: Vec<u64>,
    reconnects: u64,
    closed_after_dot: u64,
    greeting_refusals: u64,
    bytes: usize,
}

impl Intake {
    fn stream(smtp: SocketAddr, stream: u64, messages: u64, large_bytes: usize) -> Self {
        let mut intake = Self {
            deferrals: vec![0; PHASES.len()],
            ..Self::default()
        };
        let (mut connection, refused) = Stream::connect(smtp);
        intake.greeting_refusals += refused;
        for index in 0..messages {
            let body = message(stream, index, large_bytes);
            intake.bytes += body.len();
            let started = Instant::now();
            let mut attempts = 0;
            loop {
                attempts += 1;
                assert!(
                    attempts <= ATTEMPTS_MAX,
                    "message {stream}.{index} was never accepted"
                );
                let outcome = connection.attempt(&body);
                match outcome {
                    Attempt::Accepted(milliseconds) => {
                        if body.len() > SMALL_BYTES * 2 {
                            intake.accepted_large.push(milliseconds);
                        } else {
                            intake.accepted_small.push(milliseconds);
                        }
                        break;
                    }
                    Attempt::Deferred(phase, milliseconds) => {
                        let at = PHASES.iter().position(|each| *each == phase).unwrap();
                        intake.deferrals[at] += 1;
                        intake.deferral_waits.push(milliseconds);
                        // An RCPT refusal keeps the envelope; start over.
                        if connection.command(b"RSET\r\n") != Some(250) {
                            intake.reconnects += 1;
                            let (fresh, refused) = Stream::connect(smtp);
                            connection = fresh;
                            intake.greeting_refusals += refused;
                        }
                    }
                    Attempt::Closed | Attempt::ClosedAfterDot => {
                        if matches!(outcome, Attempt::ClosedAfterDot) {
                            intake.closed_after_dot += 1;
                        }
                        intake.reconnects += 1;
                        let (fresh, refused) = Stream::connect(smtp);
                        connection = fresh;
                        intake.greeting_refusals += refused;
                    }
                }
                thread::sleep(RETRY_PAUSE);
            }
            intake
                .to_accept
                .push(started.elapsed().as_secs_f64() * 1000.0);
        }
        assert_eq!(connection.command(b"QUIT\r\n"), Some(221));
        intake
    }

    fn merge(&mut self, other: Self) {
        self.accepted_small.extend(other.accepted_small);
        self.accepted_large.extend(other.accepted_large);
        self.to_accept.extend(other.to_accept);
        self.deferral_waits.extend(other.deferral_waits);
        self.deferrals.resize(PHASES.len(), 0);
        for (total, each) in self.deferrals.iter_mut().zip(other.deferrals) {
            *total += each;
        }
        self.reconnects += other.reconnects;
        self.closed_after_dot += other.closed_after_dot;
        self.greeting_refusals += other.greeting_refusals;
        self.bytes += other.bytes;
    }
}

/// Count, p50/p90/p95/p99 and max of `values`, which this sorts.
fn distribution(values: &mut [f64]) -> String {
    values.sort_by(f64::total_cmp);
    let at = |fraction: f64| {
        let rank = ((values.len().max(1) - 1) as f64 * fraction).round() as usize;
        values.get(rank).copied().unwrap_or(0.0)
    };
    format!(
        "{{\"count\":{},\"p50\":{:.1},\"p90\":{:.1},\"p95\":{:.1},\"p99\":{:.1},\"max\":{:.1}}}",
        values.len(),
        at(0.5),
        at(0.9),
        at(0.95),
        at(0.99),
        at(1.0)
    )
}

fn intake(fixture: &Fixture, smtp: SocketAddr, cloud: &Service, knobs: &Knobs) {
    let stored = sql(fixture, "SELECT count(*) FROM td_mta.emails")
        .parse::<u64>()
        .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let stop = stop.clone();
        let pid = cloud.pid();
        thread::spawn(move || {
            let mut peak = 0;
            while !stop.load(Ordering::Acquire) {
                // The process is gone only while a failed run unwinds.
                let Some((rss, _)) = try_memory(pid) else {
                    break;
                };
                peak = peak.max(rss);
                thread::sleep(Duration::from_millis(20));
            }
            peak
        })
    };
    let before = Database::read(fixture);
    let started = Instant::now();
    let workers = (0..knobs.streams)
        .map(|stream| {
            let messages = knobs.messages;
            let large_bytes = knobs.large_bytes;
            thread::spawn(move || Intake::stream(smtp, stream, messages, large_bytes))
        })
        .collect::<Vec<_>>();
    let mut intake = Intake::default();
    for worker in workers {
        intake.merge(worker.join().unwrap());
    }
    let elapsed = started.elapsed().as_secs_f64();
    stop.store(true, Ordering::Release);
    let peak = sampler.join().unwrap();
    let after = Database::read(fixture);
    let total = knobs.streams * knobs.messages;
    let duplicates = sql(fixture, "SELECT count(*) FROM td_mta.emails")
        .parse::<u64>()
        .unwrap()
        .checked_sub(stored + total)
        .unwrap_or_else(|| panic!("every accepted message is stored"));
    assert!(
        duplicates <= intake.closed_after_dot,
        "{duplicates} messages stored twice without an uncertain close"
    );
    let (_, hwm) = memory(cloud.pid());
    let deferrals = PHASES
        .iter()
        .zip(&intake.deferrals)
        .map(|(phase, count)| format!("\"{phase}\":{count}"))
        .collect::<Vec<_>>()
        .join(",");
    report(
        "smtp_intake",
        &format!(
            "\"streams\":{},\"messages\":{total},\"message_bytes\":{},\"seconds\":{elapsed:.2},\"messages_per_second\":{:.1},\"mib_per_second\":{:.2},\"accept_small_ms\":{},\"accept_large_ms\":{},\"time_to_accept_ms\":{},\"deferrals\":{{{deferrals}}},\"deferral_wait_ms\":{},\"reconnects\":{},\"closed_after_dot\":{},\"duplicates\":{duplicates},\"greeting_refusals\":{},\"cloud_sampled_peak_rss_kib\":{peak},\"cloud_hwm_kib\":{hwm},\"database\":{{{}}}",
            knobs.streams,
            intake.bytes,
            total as f64 / elapsed,
            intake.bytes as f64 / elapsed / 1_048_576.0,
            distribution(&mut intake.accepted_small),
            distribution(&mut intake.accepted_large),
            distribution(&mut intake.to_accept),
            distribution(&mut intake.deferral_waits),
            intake.reconnects,
            intake.closed_after_dot,
            intake.greeting_refusals,
            after.since(before, elapsed),
        ),
    );
}

struct Response {
    status: u16,
    body: Vec<u8>,
    milliseconds: f64,
}

/// One device credential and one fresh TLS connection per request, as
/// every td-mta response closes its connection. All clients share one pace.
struct Jmap {
    address: SocketAddr,
    ca: Vec<u8>,
    authorization: String,
    next_start: Mutex<Option<Instant>>,
}

impl Jmap {
    fn new(fixture: &Fixture, address: SocketAddr) -> Self {
        let trust = td_crypto::TrustStore::from_pem(&fixture.ca).unwrap();
        let epoch = td_mta::postgres_schema::inspect(&fixture.request(), &trust, deadline())
            .unwrap()
            .store_epoch;
        let account = td_mta::ids::AccountId::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let device =
            td_mta::postgres_devices::prepare_creation(epoch, account, "performance client", 1)
                .unwrap();
        td_mta::postgres_devices::publish_creation(&fixture.request(), &trust, deadline(), &device)
            .unwrap();
        let credentials = [b"user:".as_slice(), device.password()].concat();
        let encoded = certificate::pem("BASIC", &credentials);
        let authorization = std::str::from_utf8(&encoded)
            .unwrap()
            .lines()
            .filter(|line| !line.starts_with("---"))
            .collect::<String>();
        Self {
            address,
            ca: fixture.ca.clone(),
            authorization,
            next_start: Mutex::new(None),
        }
    }

    fn pace(&self) {
        let now = Instant::now();
        let start = {
            let mut next = self.next_start.lock().unwrap();
            let start = next.map_or(now, |next| next.max(now));
            *next = Some(start + Duration::from_secs(1) / PACED_PER_SECOND);
            start
        };
        thread::sleep(start.saturating_duration_since(now));
    }

    /// Milliseconds exclude pacing but include the fixture TLS client.
    fn send(&self, head: &str, body: &[u8]) -> Response {
        self.pace();
        let started = Instant::now();
        let (mut tls, _) = super::smtp_transport::upgrade_protocol(
            TcpStream::connect(self.address).unwrap(),
            Arc::new(td_mta::clock::RuntimeClock::new()),
            &self.ca,
            td_crypto::TlsProtocol::Http1,
        );
        write!(
            tls,
            "{head}Host: localhost:{}\r\nAuthorization: Basic {}\r\nConnection: close\r\n\r\n",
            self.address.port(),
            self.authorization
        )
        .unwrap();
        tls.write_all(body).unwrap();
        tls.flush().unwrap();
        let mut reply = Vec::new();
        tls.read_to_end(&mut reply).unwrap();
        let milliseconds = started.elapsed().as_secs_f64() * 1000.0;
        let split = reply
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap();
        let header = std::str::from_utf8(&reply[..split]).unwrap();
        let status = header.get(9..12).unwrap().parse().unwrap();
        let body = reply[split + 4..].to_vec();
        let length = header
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .map(|value| value.parse::<usize>().unwrap());
        assert_eq!(length, Some(body.len()), "{header}");
        Response {
            status,
            body,
            milliseconds,
        }
    }

    fn get(&self, target: &str) -> Response {
        self.send(&format!("GET {target} HTTP/1.1\r\n"), b"")
    }

    fn api(&self, request: &str) -> Response {
        self.send(
            &format!(
                "POST /jmap/api HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
                request.len()
            ),
            request.as_bytes(),
        )
    }
}

/// Refusals, shared by concurrent clients. An API call refused because the
/// body slot is held says so in its 429 body; a plain 429 is a download
/// refused for the same reason, or the attempt limiter, which pacing keeps
/// out; a 503 is a storage or service failure.
#[derive(Default)]
struct Busy {
    api_slot: AtomicU64,
    too_many: AtomicU64,
    unavailable: AtomicU64,
}

impl Busy {
    /// Retries a 429 or 503 after a pause; anything but 200 then fails.
    fn retry(&self, mut send: impl FnMut() -> Response) -> Response {
        for _ in 0..HTTP_ATTEMPTS_MAX {
            let response = send();
            match response.status {
                200 => return response,
                429 if response
                    .body
                    .windows(21)
                    .any(|window| window == b"maxConcurrentRequests") =>
                {
                    self.api_slot.fetch_add(1, Ordering::Relaxed)
                }
                429 => self.too_many.fetch_add(1, Ordering::Relaxed),
                503 => self.unavailable.fetch_add(1, Ordering::Relaxed),
                status => panic!("HTTP {status}: {}", String::from_utf8_lossy(&response.body)),
            };
            thread::sleep(RETRY_PAUSE);
        }
        panic!("request was never admitted");
    }

    fn fields(&self) -> String {
        format!(
            "\"429_api_slot\":{},\"429\":{},\"503\":{}",
            self.api_slot.load(Ordering::Relaxed),
            self.too_many.load(Ordering::Relaxed),
            self.unavailable.load(Ordering::Relaxed)
        )
    }
}

fn method(response: &Response, name: &str) -> td_json::Json {
    let reply = td_json::parse_slice(&response.body).unwrap();
    let call = &reply["methodResponses"][0];
    assert_eq!(call[0].as_str(), Some(name), "{reply:?}");
    call[1].clone()
}

struct Mailbox {
    account: String,
    download: String,
    /// Each message's blob and size.
    messages: Vec<(String, u64)>,
}

impl Mailbox {
    fn download_target(&self, blob: &str) -> String {
        let template = self
            .download
            .get(self.download.find("/jmap/download/").unwrap()..)
            .unwrap();
        template
            .replace("{accountId}", &self.account)
            .replace("{blobId}", blob)
            .replace("{name}", "message.eml")
            .replace("{type}", "message%2Frfc822")
    }

    fn download(&self, jmap: &Jmap, busy: &Busy, index: usize) -> Response {
        let (blob, size) = &self.messages[index];
        let response = busy.retry(|| jmap.get(&self.download_target(blob)));
        assert_eq!(response.body.len() as u64, *size, "message {blob} size");
        response
    }
}

fn get_request(account: &str, ids: &[String]) -> String {
    td_json::json!({"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[["Email/get",{"accountId":account,"ids":ids,"properties":["id","blobId","size"]},"get"]]})
    .to_string()
}

/// Session, query paging, metadata batches and every message's raw bytes,
/// one request at a time.
fn fetch(fixture: &Fixture, jmap: &Jmap, desktop: &Service, expected: u64) -> Mailbox {
    let busy = Busy::default();
    let before = Database::read(fixture);
    let started = Instant::now();
    let mut sessions = Vec::new();
    let mut session = None;
    for _ in 0..5 {
        let response = busy.retry(|| jmap.get("/.well-known/jmap"));
        sessions.push(response.milliseconds);
        session = Some(td_json::parse_slice(&response.body).unwrap());
    }
    let session = session.unwrap();
    let account = session["primaryAccounts"]["urn:ietf:params:jmap:mail"]
        .as_str()
        .unwrap()
        .to_owned();
    let download = session["downloadUrl"].as_str().unwrap().to_owned();
    let mut queries = Vec::new();
    let mut ids = Vec::new();
    loop {
        let request = td_json::json!({"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[["Email/query",{"accountId":account.as_str(),"position":ids.len(),"limit":QUERY_PAGE},"query"]]}).to_string();
        let response = busy.retry(|| jmap.api(&request));
        queries.push(response.milliseconds);
        let page = method(&response, "Email/query")["ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        let last = page.len() < QUERY_PAGE;
        ids.extend(page);
        if last {
            break;
        }
    }
    assert_eq!(ids.len() as u64, expected, "query returns every message");
    let mut gets = Vec::new();
    let mut messages = Vec::new();
    for batch in ids.chunks(GET_BATCH) {
        let request = get_request(&account, batch);
        let response = busy.retry(|| jmap.api(&request));
        gets.push(response.milliseconds);
        for email in method(&response, "Email/get")["list"].as_array().unwrap() {
            messages.push((
                email["blobId"].as_str().unwrap().to_owned(),
                email["size"].as_u64().unwrap(),
            ));
        }
    }
    assert_eq!(messages.len(), ids.len(), "every queried message is read");
    let mailbox = Mailbox {
        account,
        download,
        messages,
    };
    let mut downloads = Vec::new();
    let mut bytes = 0;
    let downloading = Instant::now();
    for index in 0..mailbox.messages.len() {
        let response = mailbox.download(jmap, &busy, index);
        bytes += response.body.len();
        downloads.push(response.milliseconds);
    }
    let download_seconds = downloading.elapsed().as_secs_f64();
    let elapsed = started.elapsed().as_secs_f64();
    let after = Database::read(fixture);
    report(
        "jmap_fetch",
        &format!(
            "\"messages\":{},\"download_bytes\":{bytes},\"seconds\":{elapsed:.2},\"session_ms\":{},\"query_page\":{QUERY_PAGE},\"query_ms\":{},\"get_batch\":{GET_BATCH},\"get_ms\":{},\"download_ms\":{},\"download_mib_per_second\":{:.2},\"busy\":{{{}}},{},\"database\":{{{}}}",
            mailbox.messages.len(),
            distribution(&mut sessions),
            distribution(&mut queries),
            distribution(&mut gets),
            distribution(&mut downloads),
            bytes as f64 / download_seconds / 1_048_576.0,
            busy.fields(),
            desktop.memory_fields(),
            after.since(before, elapsed),
        ),
    );
    mailbox
}

/// Readers download the whole mailbox at once while another client queries,
/// so downloads and API calls contend for the one body slot.
fn fetch_concurrently(
    fixture: &Fixture,
    jmap: &Arc<Jmap>,
    mailbox: &Arc<Mailbox>,
    desktop: &Service,
    readers: u64,
) {
    let busy = Arc::new(Busy::default());
    let downloading = Arc::new(AtomicBool::new(true));
    let before = Database::read(fixture);
    let started = Instant::now();
    let workers = (0..readers)
        .map(|reader| {
            let (jmap, mailbox, busy) = (jmap.clone(), mailbox.clone(), busy.clone());
            thread::spawn(move || {
                let count = mailbox.messages.len();
                let offset = count * reader as usize / readers as usize;
                let mut latencies = Vec::new();
                let mut bytes = 0;
                for step in 0..count {
                    let response = mailbox.download(&jmap, &busy, (offset + step) % count);
                    bytes += response.body.len();
                    latencies.push(response.milliseconds);
                }
                (latencies, bytes)
            })
        })
        .collect::<Vec<_>>();
    let api_busy = Arc::new(Busy::default());
    let metadata = {
        let (jmap, mailbox, busy, downloading) = (
            jmap.clone(),
            mailbox.clone(),
            api_busy.clone(),
            downloading.clone(),
        );
        thread::spawn(move || {
            let mut latencies = Vec::new();
            let request = td_json::json!({"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[["Email/query",{"accountId":mailbox.account.as_str(),"limit":GET_BATCH},"query"]]}).to_string();
            while downloading.load(Ordering::Acquire) {
                let response = busy.retry(|| jmap.api(&request));
                method(&response, "Email/query");
                latencies.push(response.milliseconds);
            }
            latencies
        })
    };
    let mut downloads = Vec::new();
    let mut bytes = 0;
    for worker in workers {
        let (latencies, count) = worker.join().unwrap();
        downloads.extend(latencies);
        bytes += count;
    }
    downloading.store(false, Ordering::Release);
    let mut queries = metadata.join().unwrap();
    let elapsed = started.elapsed().as_secs_f64();
    let after = Database::read(fixture);
    report(
        "jmap_concurrent",
        &format!(
            "\"readers\":{readers},\"downloads\":{},\"download_bytes\":{bytes},\"seconds\":{elapsed:.2},\"download_mib_per_second\":{:.2},\"download_ms\":{},\"download_busy\":{{{}}},\"query_ms\":{},\"query_busy\":{{{}}},{},\"database\":{{{}}}",
            downloads.len(),
            bytes as f64 / elapsed / 1_048_576.0,
            distribution(&mut downloads),
            busy.fields(),
            distribution(&mut queries),
            api_busy.fields(),
            desktop.memory_fields(),
            after.since(before, elapsed),
        ),
    );
}

pub(super) fn exercise(
    fixture: &Fixture,
    smtp: TcpListener,
    https: TcpListener,
    relay: TcpListener,
    dns: UdpSocket,
) {
    let knobs = Knobs::from_env();
    let smtp_address = smtp.local_addr().unwrap();
    let https_address = https.local_addr().unwrap();
    let connection = fixture.root.join("performance-connection");
    let ca = fixture.root.join("performance-ca.pem");
    fs::set_permissions(fixture.root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(&ca, &fixture.ca).unwrap();
    fs::write(
        &connection,
        format!(
            "postgresql://mail_runtime:{}@localhost:{}/postgres?sslrootcert={}\n",
            std::str::from_utf8(PASSWORD).unwrap(),
            fixture.port,
            ca.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o600)).unwrap();
    let local = td_fs::TempDir::new("perf").unwrap();
    // The services bind these addresses; the unserved relay and resolver
    // stay held so no other socket takes them while nothing is queued.
    drop((smtp, https));
    let _unserved = (relay, dns);
    report(
        "configuration",
        &format!(
            "\"idle_seconds\":{},\"smtp_streams\":{},\"smtp_messages_per_stream\":{},\"jmap_readers\":{},\"small_bytes\":{SMALL_BYTES},\"large_bytes\":{large_bytes},\"large_every\":{LARGE_EVERY},\"https_paced_per_second\":{PACED_PER_SECOND},\"profile\":\"{}\",\"target\":\"{}-{}-{}\",\"kernel\":\"{}\",\"rlimit_data\":\"{}\"",
            knobs.idle_seconds,
            knobs.streams,
            knobs.messages,
            knobs.readers,
            if cfg!(debug_assertions) { "debug" } else { "release" },
            std::env::consts::ARCH,
            std::env::consts::OS,
            if cfg!(target_env = "musl") { "musl" } else { "gnu" },
            fs::read_to_string("/proc/sys/kernel/osrelease")
                .unwrap()
                .trim(),
            data_limit(),
            large_bytes = knobs.large_bytes,
        ),
    );
    let desktop = Service::start(&local, &connection, "desktop");
    idle(fixture, "idle_desktop", &[&desktop], knobs.idle_seconds);
    let cloud = Service::start(&local, &connection, "cloud");
    idle(
        fixture,
        "idle_desktop_cloud",
        &[&desktop, &cloud],
        knobs.idle_seconds,
    );
    intake(fixture, smtp_address, &cloud, &knobs);
    let jmap = Arc::new(Jmap::new(fixture, https_address));
    let stored = sql(fixture, "SELECT count(*) FROM td_mta.emails")
        .parse::<u64>()
        .unwrap();
    let mailbox = Arc::new(fetch(fixture, &jmap, &desktop, stored));
    fetch_concurrently(fixture, &jmap, &mailbox, &desktop, knobs.readers);
    idle(
        fixture,
        "idle_after_intake",
        &[&desktop, &cloud],
        knobs.idle_seconds,
    );
}
