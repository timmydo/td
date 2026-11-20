//! Deadline-bounded system resolution in an owned, self-expiring helper.
use crate::postgres_bootstrap::{valid_host, Bootstrap};
use crate::postgres_probe::Endpoint;
use std::{
    io::{self, Read, Write},
    net::{IpAddr, ToSocketAddrs},
    path::Path,
    process::{Child, Command, Stdio},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

const MAX_DURATION: Duration = Duration::from_secs(5);
// getaddrinfo supplies no authoritative DNS TTL.
const REUSE_WINDOW: Duration = Duration::from_secs(1);
const MAX_ADDRESSES: usize = 16;
const MAX_REPLY: usize = 1024;
const HEADER: &str = "td-mta-dns-1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Deadline,
    Spawn,
    Resolver,
    Protocol,
    Capacity,
}

/// executable is the caller's trusted td-mta binary, not a shell or command.
/// A parsed TCP bootstrap owns one short-lived positive answer.
pub fn resolve(
    executable: &Path,
    bootstrap: &Bootstrap,
    deadline: Instant,
) -> Result<Vec<Endpoint>, Error> {
    if let Some(path) = bootstrap.socket() {
        remaining_time(deadline)?;
        return Ok(vec![Endpoint::Unix(*path)]);
    }
    if !executable.is_absolute() || remaining(deadline)? > MAX_DURATION {
        return Err(Error::Invalid);
    }
    let host = bootstrap.host().ok_or(Error::Invalid)?;
    let endpoints = bootstrap
        .addresses
        .resolve(Instant::now, bootstrap.port(), || {
            resolve_host(executable, host, deadline)
        })?;
    remaining_time(deadline)?;
    Ok(endpoints)
}

struct Answer {
    started: Instant,
    addresses: Vec<IpAddr>,
}

#[derive(Default)]
pub(crate) struct Cache(Mutex<Option<Answer>>);

impl Cache {
    fn resolve(
        &self,
        clock: impl Fn() -> Instant,
        port: u16,
        lookup: impl FnOnce() -> Result<Vec<IpAddr>, Error>,
    ) -> Result<Vec<Endpoint>, Error> {
        if let Ok(current) = self.0.try_lock() {
            if let Some(answer) = current.as_ref() {
                if clock()
                    .checked_duration_since(answer.started)
                    .is_some_and(|age| age < REUSE_WINDOW)
                {
                    return endpoints(&answer.addresses, port);
                }
            }
        }
        // Resolve outside the lock. Callers keep independent deadlines on a miss.
        let started = clock();
        let addresses = lookup()?;
        let endpoints = endpoints(&addresses, port)?;
        if let Ok(mut current) = self.0.try_lock() {
            if current
                .as_ref()
                .is_none_or(|answer| answer.started <= started)
            {
                *current = Some(Answer { started, addresses });
            }
        }
        Ok(endpoints)
    }
}

fn endpoints(addresses: &[IpAddr], port: u16) -> Result<Vec<Endpoint>, Error> {
    let mut endpoints = td_alloc::with_capacity(addresses.len()).map_err(|_| Error::Capacity)?;
    for &address in addresses {
        td_alloc::push(&mut endpoints, Endpoint::Tcp((address, port).into()))
            .map_err(|_| Error::Capacity)?;
    }
    Ok(endpoints)
}

/// Bounded resolution for one already-authorized service endpoint.
pub fn resolve_host(
    executable: &Path,
    host: &str,
    deadline: Instant,
) -> Result<Vec<IpAddr>, Error> {
    if !executable.is_absolute() {
        return Err(Error::Invalid);
    }
    let mut timing = crate::performance::Timer::start(crate::performance::Phase::DbResolve);
    let mut command = Command::new(executable);
    command.arg("--internal-postgres-resolve");
    let result = resolve_command(command, host, deadline);
    if result.is_ok() {
        timing.succeeded();
    }
    result
}

fn resolve_command(
    mut command: Command,
    host: &str,
    deadline: Instant,
) -> Result<Vec<IpAddr>, Error> {
    let remaining = remaining(deadline)?;
    if remaining > MAX_DURATION || !valid_host(host) {
        return Err(Error::Invalid);
    }
    let child = command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Error::Spawn)?;
    let mut child = Reap {
        child,
        exited: false,
    };
    remaining_time(deadline)?;
    {
        let mut input = child.child.stdin.take().ok_or(Error::Protocol)?;
        // At most 253 bytes fit in the empty pipe without a helper read.
        input
            .write_all(host.as_bytes())
            .map_err(|_| Error::Resolver)?;
    }
    loop {
        remaining_time(deadline)?;
        if let Some(status) = child.child.try_wait().map_err(|_| Error::Resolver)? {
            child.exited = true;
            if !status.success() {
                return Err(Error::Resolver);
            }
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    // The trusted helper writes at most MAX_REPLY bytes, then exits with no
    // descendants. Its complete reply fits in the pipe before we read it.
    let output = child.child.stdout.take().ok_or(Error::Protocol)?;
    let mut bytes = Vec::with_capacity(MAX_REPLY + 1);
    output
        .take((MAX_REPLY + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Protocol)?;
    remaining_time(deadline)?;
    let addresses = parse_reply(&bytes)?;
    remaining_time(deadline)?;
    Ok(addresses)
}

fn remaining(deadline: Instant) -> Result<Duration, Error> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(Error::Deadline)
}
fn remaining_time(deadline: Instant) -> Result<(), Error> {
    remaining(deadline).map(|_| ())
}

struct Reap {
    child: Child,
    exited: bool,
}
impl Drop for Reap {
    fn drop(&mut self) {
        if !self.exited {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn parse_reply(bytes: &[u8]) -> Result<Vec<IpAddr>, Error> {
    if bytes.len() > MAX_REPLY || !bytes.ends_with(b"\n") {
        return Err(Error::Protocol);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| Error::Protocol)?;
    let mut lines = text.split_terminator('\n');
    if lines.next() != Some(HEADER) {
        return Err(Error::Protocol);
    }
    let mut addresses = Vec::with_capacity(MAX_ADDRESSES);
    for line in lines {
        let address = line.parse::<IpAddr>().map_err(|_| Error::Protocol)?;
        if addresses.len() == MAX_ADDRESSES || addresses.contains(&address) {
            return Err(Error::Protocol);
        }
        addresses.push(address);
    }
    if addresses.is_empty() {
        return Err(Error::Resolver);
    }
    Ok(addresses)
}

fn watchdog(duration: Duration) -> io::Result<()> {
    // This process exists only to resolve one name. Exit terminates blocked
    // NSS work too under normal platform exit cleanup after parent loss.
    let _watchdog = thread::Builder::new()
        .name("dns-expiry".into())
        .spawn(move || {
            thread::sleep(duration);
            std::process::exit(1);
        })?;
    Ok(())
}

/// Private executable mode. No credentials, provider errors or hostnames are
/// emitted. The parent starts this mode before any service/configuration work.
pub fn run_helper_and_exit() -> ! {
    if watchdog(MAX_DURATION).is_err() {
        std::process::exit(1);
    }
    let result = helper_reply(&mut io::stdin().lock(), &mut io::stdout().lock());
    std::process::exit(if result.is_ok() { 0 } else { 1 });
}

fn helper_reply(input: &mut impl Read, output: &mut impl Write) -> Result<(), Error> {
    let mut bytes = Vec::with_capacity(254);
    input
        .take(254)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Resolver)?;
    let host = std::str::from_utf8(&bytes).map_err(|_| Error::Invalid)?;
    if !valid_host(host) {
        return Err(Error::Invalid);
    }
    let resolved = (host, 0).to_socket_addrs().map_err(|_| Error::Resolver)?;
    let mut addresses = Vec::with_capacity(MAX_ADDRESSES);
    // The system resolver's own memory/work belongs to this disposable
    // process; cap our traversal as well as the retained unique results.
    for (position, address) in resolved.enumerate() {
        if position == 128 {
            return Err(Error::Resolver);
        }
        if let std::net::SocketAddr::V6(address) = address {
            if address.scope_id() != 0 {
                return Err(Error::Resolver);
            }
        }
        let address = address.ip();
        if !addresses.contains(&address) {
            if addresses.len() == MAX_ADDRESSES {
                return Err(Error::Resolver);
            }
            addresses.push(address);
        }
    }
    if addresses.is_empty() {
        return Err(Error::Resolver);
    }
    let mut reply = format!("{HEADER}\n");
    for address in addresses {
        use std::fmt::Write as _;
        writeln!(reply, "{address}").map_err(|_| Error::Protocol)?;
    }
    if reply.len() > MAX_REPLY {
        return Err(Error::Protocol);
    }
    output
        .write_all(reply.as_bytes())
        .map_err(|_| Error::Protocol)?;
    output.flush().map_err(|_| Error::Protocol)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn positive_answers_expire_from_lookup_start_and_errors_are_not_retained() {
        let cache = Cache::default();
        let first = IpAddr::from([192, 0, 2, 1]);
        let next = IpAddr::from([192, 0, 2, 2]);
        let now = Instant::now();
        let deadline = now + REUSE_WINDOW;
        let cold = cache
            .resolve(|| now, 1234, || Ok(vec![next, first]))
            .unwrap();
        assert_eq!(cold, endpoints(&[next, first], 1234).unwrap());
        assert_eq!(
            td_alloc::with_reservation_limit(0, || cache.resolve(
                || now,
                1234,
                || Err(Error::Spawn)
            )),
            Err(Error::Capacity)
        );
        assert_eq!(
            cache.resolve(
                || deadline - Duration::from_nanos(1),
                1234,
                || Err(Error::Spawn)
            ),
            Ok(cold),
        );
        assert_eq!(
            cache.resolve(|| deadline, 1234, || Err(Error::Resolver)),
            Err(Error::Resolver)
        );
        assert_eq!(
            cache
                .resolve(|| deadline, 1234, || Ok(vec![first]))
                .unwrap(),
            endpoints(&[first], 1234).unwrap()
        );
        let other = Cache::default();
        assert_eq!(
            other.resolve(|| deadline, 1234, || Err(Error::Spawn)),
            Err(Error::Spawn)
        );
    }

    #[test]
    fn slow_lookup_does_not_get_another_window_on_completion() {
        let cache = Cache::default();
        let now = std::cell::Cell::new(Instant::now());
        let first = IpAddr::from([192, 0, 2, 1]);
        cache
            .resolve(
                || now.get(),
                5432,
                || {
                    now.set(now.get() + REUSE_WINDOW);
                    Ok(vec![first])
                },
            )
            .unwrap();
        assert_eq!(
            cache.resolve(|| now.get(), 5432, || Err(Error::Spawn)),
            Err(Error::Spawn)
        );
    }

    #[test]
    fn a_warm_bootstrap_still_enforces_the_callers_limits() {
        let bootstrap = Bootstrap::parse("postgresql://u:p@localhost/db", "frontend").unwrap();
        let first = IpAddr::from([192, 0, 2, 1]);
        let now = Instant::now();
        let expected = bootstrap
            .addresses
            .resolve(|| now, 5432, || Ok(vec![first]))
            .unwrap();
        let absent_helper = Path::new("/unused-helper");
        assert_eq!(
            resolve(absent_helper, &bootstrap, now),
            Err(Error::Deadline)
        );
        let too_long = now + MAX_DURATION + Duration::from_secs(5);
        assert_eq!(
            resolve(absent_helper, &bootstrap, too_long),
            Err(Error::Invalid)
        );
        let allowed = now + MAX_DURATION;
        assert_eq!(
            resolve(Path::new("td-mta"), &bootstrap, allowed),
            Err(Error::Invalid)
        );
        assert_eq!(resolve(absent_helper, &bootstrap, allowed), Ok(expected));
    }

    #[test]
    fn overlapping_lookups_keep_the_newer_answer() {
        let cache = Cache::default();
        let first = IpAddr::from([192, 0, 2, 1]);
        let next = IpAddr::from([192, 0, 2, 2]);
        let now = Instant::now();
        let later = now + Duration::from_millis(10);
        cache
            .resolve(
                || now,
                5432,
                || {
                    cache.resolve(|| later, 5432, || Ok(vec![next]))?;
                    Ok(vec![first])
                },
            )
            .unwrap();
        let cached = cache.resolve(|| later, 5432, || Err(Error::Spawn)).unwrap();
        assert_eq!(cached, endpoints(&[next], 5432).unwrap());
    }

    #[test]
    fn reply_is_complete_bounded_unique_and_typed() {
        let good = parse_reply(b"td-mta-dns-1\n127.0.0.1\n::1\n").unwrap();
        assert_eq!(good.len(), 2);
        for bad in [
            b"".as_slice(),
            b"td-mta-dns-1\n",
            b"td-mta-dns-1\n127.0.0.1",
            b"td-mta-dns-1\n127.1\n",
            b"td-mta-dns-1\n127.0.0.1\n127.0.0.1\n",
            b"other\n127.0.0.1\n",
            b"td-mta-dns-1\n127.0.0.1\n\n",
        ] {
            assert!(parse_reply(bad).is_err());
        }
        let mut many = format!("{HEADER}\n");
        for suffix in 1..=MAX_ADDRESSES + 1 {
            use std::fmt::Write as _;
            writeln!(many, "127.0.0.{suffix}").unwrap();
        }
        assert!(parse_reply(many.as_bytes()).is_err());
        assert!(parse_reply(&vec![b'x'; MAX_REPLY + 1]).is_err());
    }

    #[test]
    fn malformed_helper_input_emits_nothing() {
        for input in [b"localhost\n".as_slice(), b"127.1", b"localhost\0", b""] {
            let mut output = Vec::new();
            assert!(helper_reply(&mut &input[..], &mut output).is_err());
            assert!(output.is_empty());
        }
        let input = vec![b'a'; 254];
        assert!(helper_reply(&mut &input[..], &mut Vec::new()).is_err());
    }

    #[test]
    fn parent_deadline_kills_and_reaps_stalled_helper() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "postgres_resolver::tests::stalled_helper",
            "--ignored",
        ]);
        let start = Instant::now();
        assert_eq!(
            resolve_command(command, "localhost", start + Duration::from_millis(50)),
            Err(Error::Deadline)
        );
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    #[ignore = "child process fixture; only parent_deadline invokes it"]
    fn stalled_helper() {
        thread::sleep(Duration::from_secs(5));
    }

    #[test]
    fn child_watchdog_exits_without_parent_cancellation() {
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "postgres_resolver::tests::expiring_helper",
                "--ignored",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut child = Reap {
            child,
            exited: false,
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = child.child.try_wait().unwrap() {
                child.exited = true;
                assert!(!status.success());
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    #[ignore = "child process fixture; only child_watchdog invokes it"]
    fn expiring_helper() {
        watchdog(Duration::from_millis(50)).unwrap();
        thread::sleep(Duration::from_secs(5));
    }
}
