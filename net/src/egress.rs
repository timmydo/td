// td-egressd: the egress relay (td-agent/DESIGN.md §10; APPLICATIONS.md
// §W.8, "The egress relay"). td-agent's one way to a destination its
// in-jail proxy admitted: a connection on its socket names a host and
// port; the relay resolves the name, refuses the whole request when any
// address it resolves to is one no workspace may reach, connects to the
// first admitted address that answers, and then splices bytes both ways
// under an idle and a total deadline. It holds no other policy: which
// destinations a workspace may reach is td-agent's.
//
// One request per connection over a Unix socket, mode 0600 in a 0700
// directory as fetchd's is:
//
//   td-egress 1
//   connect HOST PORT        (or: probe)
//   <blank line>
//
// answered `td-egress 1`, then `ok` or `error KIND: why`, then a blank
// line; after `ok` to a connect, the connection is the destination's.
// A name is resolved here, never in the jail, and only once: the address
// checked is the address connected to.
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const PROTOCOL: &str = "td-egress 1";
/// The longest line of a request's head.
const MAX_LINE: usize = 512;
/// The most lines a head holds, its blank line included.
const MAX_LINES: usize = 3;
/// How many connections are relayed at once; one more waits.
const WORKERS: usize = 64;
/// How long a client has to send its head.
const HEAD_TIME: Duration = Duration::from_secs(30);
/// How many name lookups may be under way, those given up on included.
const MAX_LOOKUPS: usize = 32;
/// How long a name may take to resolve.
const RESOLVE_TIME: Duration = Duration::from_secs(10);
/// How long one address may take to answer.
const CONNECT_TIME: Duration = Duration::from_secs(10);
/// How long the addresses a name resolves to may take to answer, all
/// told.
const CONNECT_TOTAL: Duration = Duration::from_secs(30);
/// How long a relayed connection may move no byte either way.
const IDLE: Duration = Duration::from_secs(300);
/// How long a relayed connection may last at all.
const TOTAL: Duration = Duration::from_secs(60 * 60);
/// The bytes each direction of a splice copies at a time.
const CHUNK: usize = 64 * 1024;

/// What the relay may reach that a workspace never may: loopback, for
/// the tests alone, which serve on it; never set by a launch or a unit.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Policy {
    pub(crate) allow_loopback: bool,
}

pub fn run(args: &[String]) {
    let code = match args.get(1).map(String::as_str) {
        Some("run") => match parse_run_args(args.get(2..).unwrap_or(&[])) {
            Ok((socket, policy, parent)) => match serve(Path::new(&socket), policy, parent) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("td-egressd: {e}");
                    1
                }
            },
            Err(e) => {
                eprintln!("td-egressd: {e}");
                2
            }
        },
        Some("probe") => match args.get(2) {
            Some(socket) if args.len() == 3 => match probe_within(Path::new(socket), HEAD_TIME) {
                Ok(()) => {
                    println!("td-egressd: ok");
                    0
                }
                Err(e) => {
                    eprintln!("td-egressd: probe {socket}: {e}");
                    1
                }
            },
            _ => {
                eprintln!("usage: td-egressd probe SOCKET");
                2
            }
        },
        _ => {
            eprintln!(
                "usage: td-egressd run --socket PATH [--allow-loopback] [--exit-with-parent PID]\n       td-egressd probe PATH"
            );
            2
        }
    };
    std::process::exit(code);
}

fn parse_run_args(args: &[String]) -> Result<(String, Policy, Option<u32>), String> {
    let mut socket = None;
    let mut parent = None;
    let mut policy = Policy {
        allow_loopback: false,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--socket" => {
                socket = Some(it.next().ok_or("--socket needs a path")?.clone());
            }
            // For the tests, which serve on loopback: never set by a launch.
            "--allow-loopback" => policy.allow_loopback = true,
            // For td-launch: it ends when the launched program does.
            "--exit-with-parent" => {
                let pid = it.next().ok_or("--exit-with-parent needs a pid")?;
                parent = Some(
                    pid.parse::<u32>()
                        .map_err(|_| format!("--exit-with-parent {pid:?} is not a pid"))?,
                );
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok((socket.ok_or("--socket is required")?, policy, parent))
}

fn serve(socket: &Path, policy: Policy, parent: Option<u32>) -> Result<(), String> {
    let listener = crate::fetchd::bind(socket)?;
    if let Some(parent) = parent {
        crate::fetchd::watch_parent(socket, parent)?;
    }
    let active = Arc::new((Mutex::new(0usize), Condvar::new()));
    loop {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) => {
                eprintln!("td-egressd: accept: {e}");
                continue;
            }
        };
        let slot = Slot::take(&active)?;
        let spawned = std::thread::Builder::new().spawn(move || {
            let _slot = slot;
            handle(stream, policy);
        });
        if let Err(e) = spawned {
            eprintln!("td-egressd: spawn worker: {e}");
        }
    }
}

/// One of the `WORKERS` places, given back when dropped.
struct Slot(Arc<(Mutex<usize>, Condvar)>);

impl Slot {
    fn take(active: &Arc<(Mutex<usize>, Condvar)>) -> Result<Slot, String> {
        let (count, wake) = &**active;
        let mut count = count.lock().map_err(|_| "worker count poisoned")?;
        while *count >= WORKERS {
            count = wake.wait(count).map_err(|_| "worker count poisoned")?;
        }
        *count += 1;
        Ok(Slot(Arc::clone(active)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let (count, wake) = &*self.0;
        if let Ok(mut count) = count.lock() {
            *count = count.saturating_sub(1);
        }
        wake.notify_one();
    }
}

/// What a request asks.
#[derive(Debug, PartialEq, Eq)]
enum Asked {
    Probe,
    Connect(Host, u16),
}

/// A destination's host: an address, or a name to resolve.
#[derive(Debug, PartialEq, Eq)]
enum Host {
    Address(IpAddr),
    Name(String),
}

/// One relayed connection: its head read, answered, and, for a connect
/// admitted, spliced until either side ends or a deadline passes.
fn handle(client: UnixStream, policy: Policy) {
    handle_within(client, policy, DEADLINES);
}

fn handle_within(mut client: UnixStream, policy: Policy, deadlines: Deadlines) {
    let asked = read_head(&mut client);
    let (host, port) = match asked {
        Ok(Asked::Probe) => {
            let _ = reply(&mut client, "ok");
            return;
        }
        Ok(Asked::Connect(host, port)) => (host, port),
        Err(why) => {
            let _ = reply(&mut client, &format!("error malformed: {why}"));
            return;
        }
    };
    let origin = match connect(&host, port, policy) {
        Ok(origin) => origin,
        Err(fault) => {
            let _ = reply(&mut client, &fault);
            return;
        }
    };
    if reply(&mut client, "ok").is_err() {
        return;
    }
    splice(client, origin, deadlines);
}

/// The request's head, read a byte at a time so nothing past its blank
/// line is taken, within `HEAD_TIME`.
fn read_head(client: &mut UnixStream) -> Result<Asked, String> {
    let deadline = Instant::now() + HEAD_TIME;
    let mut lines: Vec<String> = Vec::new();
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or("the head took too long")?;
        client
            .set_read_timeout(Some(left))
            .map_err(|e| e.to_string())?;
        match client.read(&mut byte) {
            Ok(0) => return Err("the head ended early".into()),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("reading the head: {e}")),
        }
        if byte[0] != b'\n' {
            if line.len() >= MAX_LINE {
                return Err(format!("a line is past {MAX_LINE} bytes"));
            }
            line.push(byte[0]);
            continue;
        }
        let text = String::from_utf8(std::mem::take(&mut line))
            .map_err(|_| "a line is not UTF-8".to_string())?;
        if text.is_empty() {
            break;
        }
        if lines.len() + 1 >= MAX_LINES {
            return Err("too many lines".into());
        }
        lines.push(text);
    }
    parse_head(&lines)
}

fn parse_head(lines: &[String]) -> Result<Asked, String> {
    match lines {
        [protocol, ..] if protocol != PROTOCOL => Err(format!("not {PROTOCOL:?}")),
        [_, probe] if probe == "probe" => Ok(Asked::Probe),
        [_, connect] => {
            let rest = connect
                .strip_prefix("connect ")
                .ok_or("neither a connect nor a probe")?;
            let (host, port) = rest
                .rsplit_once(' ')
                .ok_or("a connect names a host and a port")?;
            let port = Some(port)
                .filter(|port| (1..=5).contains(&port.len()))
                .filter(|port| port.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|port| port.parse::<u16>().ok())
                .filter(|port| *port != 0)
                .ok_or_else(|| format!("{port:?} is not a port"))?;
            Ok(Asked::Connect(parse_host(host)?, port))
        }
        _ => Err("a head is the protocol line and one request".into()),
    }
}

/// A host as a request names it: an IPv4 address, an IPv6 address in
/// brackets or not, or a DNS name of letters, digits, hyphens and dots.
fn parse_host(text: &str) -> Result<Host, String> {
    if let Some(inner) = text.strip_prefix('[') {
        return inner
            .strip_suffix(']')
            .and_then(|inner| inner.parse::<Ipv6Addr>().ok())
            .map(|v6| Host::Address(IpAddr::V6(v6)))
            .ok_or_else(|| format!("{text:?} is not an IPv6 address in brackets"));
    }
    if let Ok(ip) = text.parse::<IpAddr>() {
        return Ok(Host::Address(ip));
    }
    let name = text.strip_suffix('.').unwrap_or(text);
    let label_ok = |label: &str| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    if name.is_empty() || name.len() > 253 || !name.split('.').all(label_ok) {
        return Err(format!("{text:?} is not a host"));
    }
    Ok(Host::Name(name.to_ascii_lowercase()))
}

/// The destination connected, or the fault to answer with: every address
/// the host stands for checked first, and the whole request refused when
/// any one is refused, so a name that also resolves somewhere private
/// reaches nothing; then each tried in turn, all within `CONNECT_TOTAL`.
fn connect(host: &Host, port: u16, policy: Policy) -> Result<TcpStream, String> {
    let addresses = match host {
        Host::Address(ip) => vec![SocketAddr::new(*ip, port)],
        Host::Name(name) => resolve(name, port)?,
    };
    if addresses.is_empty() {
        return Err("error transport: the name resolves to no address".into());
    }
    let local = Local::read().map_err(|e| {
        format!("error refused: this machine's own networks could not be read: {e}")
    })?;
    for address in &addresses {
        if let Some(why) = refused(address.ip(), &local, policy) {
            return Err(format!("error refused: {} is {why}", address.ip()));
        }
    }
    let deadline = Instant::now() + CONNECT_TOTAL;
    let mut failed = Vec::new();
    for address in &addresses {
        let Some(left) = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
        else {
            failed.push(format!(
                "no time left of {} s for the rest",
                CONNECT_TOTAL.as_secs()
            ));
            break;
        };
        match TcpStream::connect_timeout(address, left.min(CONNECT_TIME)) {
            Ok(stream) => return Ok(stream),
            Err(e) => failed.push(format!("{address}: {e}")),
        }
    }
    Err(format!("error transport: {}", failed.join("; ")))
}

/// `name`'s addresses, within `RESOLVE_TIME`: the lookup runs on a thread
/// of its own, which a lookup that never answers is left to.
fn resolve(name: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    let lookup = Lookup::take()?;
    let (tell, told) = mpsc::channel();
    let asked = name.to_string();
    std::thread::Builder::new()
        .spawn(move || {
            let _lookup = lookup;
            let found = (asked.as_str(), port)
                .to_socket_addrs()
                .map(|found| found.collect::<Vec<_>>());
            let _ = tell.send(found);
        })
        .map_err(|e| format!("error transport: cannot resolve: {e}"))?;
    match told.recv_timeout(RESOLVE_TIME) {
        Ok(Ok(found)) => Ok(found),
        Ok(Err(e)) => Err(format!("error transport: resolving {name}: {e}")),
        Err(_) => Err(format!(
            "error transport: resolving {name} took more than {} s",
            RESOLVE_TIME.as_secs()
        )),
    }
}

/// How many lookups are under way, those given up on included.
static LOOKUPS: AtomicUsize = AtomicUsize::new(0);

/// One of `MAX_LOOKUPS` lookups under way, given back when its thread
/// ends, so lookups that never answer, each left a thread, are bounded.
struct Lookup;

impl Lookup {
    fn take() -> Result<Lookup, String> {
        let taken = LOOKUPS.fetch_add(1, Ordering::AcqRel);
        if taken >= MAX_LOOKUPS {
            LOOKUPS.fetch_sub(1, Ordering::AcqRel);
            return Err(format!(
                "error transport: {MAX_LOOKUPS} name lookups are already under way"
            ));
        }
        Ok(Lookup)
    }
}

impl Drop for Lookup {
    fn drop(&mut self) {
        LOOKUPS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Why no workspace may reach `ip`, or `None`: loopback, unless `policy`
/// allows it; in `local`, this machine's own addresses and networks;
/// unspecified, link-local, broadcast, multicast or reserved; RFC 1918
/// private, carrier-grade NAT, IPv6 unique-local or site-local; one of
/// the special ranges of `refused_v4`; or an IPv6 address carrying an
/// IPv4 address that is any of those (`embedded`), and NAT64's local-use
/// prefix whole, since where its IPv4 address lies depends on the
/// network.
pub(crate) fn refused(ip: IpAddr, local: &Local, policy: Policy) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => refused_v4(v4, local, policy),
        IpAddr::V6(v6) => refused_v6(v6, local, policy),
    }
}

/// A cloud host's platform endpoint, which answers its own machines.
const PLATFORM: Ipv4Addr = Ipv4Addr::new(168, 63, 129, 16);

fn refused_v4(v4: Ipv4Addr, local: &Local, policy: Policy) -> Option<&'static str> {
    let [a, b, c, _] = v4.octets();
    if a == 127 {
        return (!policy.allow_loopback).then_some("loopback");
    }
    if local.holds_v4(v4) {
        return Some("one of this machine's own addresses or networks");
    }
    if a == 0 {
        return Some("in 0.0.0.0/8, this network");
    }
    if a == 10 || (a == 172 && (16..32).contains(&b)) || (a == 192 && b == 168) {
        return Some("private (RFC 1918)");
    }
    if a == 100 && (64..128).contains(&b) {
        return Some("carrier-grade NAT (RFC 6598)");
    }
    if a == 169 && b == 254 {
        return Some("link-local");
    }
    if a == 192 && b == 0 && c == 0 {
        return Some("in 192.0.0.0/24, protocol assignments");
    }
    if a == 198 && (b == 18 || b == 19) {
        return Some("in 198.18.0.0/15, which local proxies map names into");
    }
    if v4 == PLATFORM {
        return Some("a cloud host's platform endpoint");
    }
    if a >= 224 {
        return Some("multicast, reserved or broadcast");
    }
    None
}

fn refused_v6(v6: Ipv6Addr, local: &Local, policy: Policy) -> Option<&'static str> {
    let s = v6.segments();
    if v6.is_unspecified() {
        return Some("unspecified");
    }
    if v6.is_loopback() {
        return (!policy.allow_loopback).then_some("loopback");
    }
    if local.holds_v6(v6) {
        return Some("one of this machine's own addresses or networks");
    }
    let first = s.first().copied().unwrap_or_default();
    if first & 0xff00 == 0xff00 {
        return Some("multicast");
    }
    if first & 0xffc0 == 0xfe80 {
        return Some("link-local");
    }
    if first & 0xffc0 == 0xfec0 {
        return Some("site-local");
    }
    if first & 0xfe00 == 0xfc00 {
        return Some("unique-local");
    }
    if s.get(..3) == Some(&[0x64, 0xff9b, 1]) {
        return Some("a local-use NAT64 address, whose IPv4 address depends on the network");
    }
    embedded(v6)
        .into_iter()
        .find_map(|v4| refused_v4(v4, local, policy))
}

/// The IPv4 addresses `v6` carries: IPv4-mapped (`::ffff:a.b.c.d`),
/// SIIT's translated form (`::ffff:0:a.b.c.d`), IPv4-compatible
/// (`::a.b.c.d`), NAT64's well-known prefix (`64:ff9b::/96`), 6to4
/// (`2002:AABB:CCDD::/48`), and Teredo's server and client
/// (`2001:0:SSSS:SSSS:…:CCCC:CCCC`, the client's inverted).
fn embedded(v6: Ipv6Addr) -> Vec<Ipv4Addr> {
    let [s0, s1, s2, s3, s4, s5, _, _] = v6.segments();
    let [_, _, o2, o3, o4, o5, o6, o7, _, _, _, _, o12, o13, o14, o15] = v6.octets();
    let last = Ipv4Addr::new(o12, o13, o14, o15);
    let mut found = Vec::new();
    if [s0, s1, s2, s3, s4] == [0; 5] && (s5 == 0xffff || s5 == 0) {
        found.push(last);
    }
    if [s0, s1, s2, s3, s4, s5] == [0, 0, 0, 0, 0xffff, 0] {
        found.push(last);
    }
    if [s0, s1, s2, s3, s4, s5] == [0x64, 0xff9b, 0, 0, 0, 0] {
        found.push(last);
    }
    if s0 == 0x2002 {
        found.push(Ipv4Addr::new(o2, o3, o4, o5));
    }
    if s0 == 0x2001 && s1 == 0 {
        found.push(Ipv4Addr::new(o4, o5, o6, o7));
        found.push(Ipv4Addr::new(!o12, !o13, !o14, !o15));
    }
    found
}

/// The shortest IPv4 and IPv6 prefixes taken as this machine's networks:
/// a shorter one routed straight to a link is a tunnel's way to the
/// whole network (a VPN's default route, or its two halves), not a
/// network of neighbours.
const MIN_V4_PREFIX: u8 = 8;
const MIN_V6_PREFIX: u8 = 16;

/// This machine's own addresses and networks, as its kernel lists them
/// now, each an address and a prefix length.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Local {
    v4: Vec<(Ipv4Addr, u8)>,
    v6: Vec<(Ipv6Addr, u8)>,
}

impl Local {
    /// IPv4: every local route (its own addresses, and a prefix routed
    /// to it whole) and every network routed straight to a link, from
    /// `/proc/net/fib_trie`. IPv6: every interface's address, from
    /// `/proc/net/if_inet6`, and every route with no next hop, from
    /// `/proc/net/ipv6_route`.
    fn read() -> Result<Local, String> {
        let read = |path: &str| std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"));
        // A kernel built without IPv6 has no such files, and no such
        // address or network.
        let optional = |path: &str| match std::fs::read_to_string(path) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(format!("{path}: {e}")),
        };
        let mut v6 = addresses_v6(&optional("/proc/net/if_inet6")?);
        let networks = networks_v6(&optional("/proc/net/ipv6_route")?, &v6);
        v6.extend(networks);
        Ok(Local {
            v4: networks_v4(&read("/proc/net/fib_trie")?),
            v6,
        })
    }

    fn holds_v4(&self, ip: Ipv4Addr) -> bool {
        self.v4.iter().any(|(net, len)| within_v4(ip, *net, *len))
    }

    fn holds_v6(&self, ip: Ipv6Addr) -> bool {
        self.v6.iter().any(|(net, len)| within_v6(ip, *net, *len))
    }
}

/// Whether `ip` is in `net`/`len`, `len` at most 32.
fn within_v4(ip: Ipv4Addr, net: Ipv4Addr, len: u8) -> bool {
    let mask = u32::MAX
        .checked_shl(32u32.saturating_sub(u32::from(len)))
        .unwrap_or(0);
    u32::from(ip) & mask == u32::from(net) & mask
}

/// Whether `ip` is in `net`/`len`, `len` at most 128.
fn within_v6(ip: Ipv6Addr, net: Ipv6Addr, len: u8) -> bool {
    let mask = u128::MAX
        .checked_shl(128u32.saturating_sub(u32::from(len)))
        .unwrap_or(0);
    u128::from(ip) & mask == u128::from(net) & mask
}

/// From `/proc/net/fib_trie`: each `|-- ADDRESS` leaf's route lines,
/// `/LENGTH SCOPE TYPE`, perhaps with a `tos=` after, of a length at
/// least `MIN_V4_PREFIX`: every `LOCAL` one, this machine's addresses
/// and prefixes routed to it whole; and every one of scope `link` and
/// type `UNICAST` that holds one of those, a network the machine is on,
/// and not a tunnel's route to the far side.
fn networks_v4(trie: &str) -> Vec<(Ipv4Addr, u8)> {
    let mut found = Vec::new();
    let mut links = Vec::new();
    let mut leaf: Option<Ipv4Addr> = None;
    for line in trie.lines() {
        let line = line.trim();
        if let Some(address) = line.strip_prefix("|-- ") {
            leaf = address.trim().parse().ok();
            continue;
        }
        let mut words = line.split_whitespace();
        let (Some(length), Some(scope), Some(kind)) = (words.next(), words.next(), words.next())
        else {
            continue;
        };
        let Some(length) = length
            .strip_prefix('/')
            .and_then(|length| length.parse::<u8>().ok())
            .filter(|length| (MIN_V4_PREFIX..=32).contains(length))
        else {
            continue;
        };
        let Some(address) = leaf else {
            continue;
        };
        let into = if kind == "LOCAL" {
            &mut found
        } else if scope == "link" && kind == "UNICAST" {
            &mut links
        } else {
            continue;
        };
        if !into.contains(&(address, length)) {
            into.push((address, length));
        }
    }
    let ours: Vec<(Ipv4Addr, u8)> = links
        .into_iter()
        .filter(|(net, len)| found.iter().any(|(own, _)| within_v4(*own, *net, *len)))
        .collect();
    found.extend(ours);
    found
}

/// From `/proc/net/if_inet6`: each address, its first field's 32
/// hexadecimal digits, as a /128.
fn addresses_v6(listed: &str) -> Vec<(Ipv6Addr, u8)> {
    listed
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter_map(hex_v6)
        .map(|address| (address, 128))
        .collect()
}

/// From `/proc/net/ipv6_route`: each route, `DEST LENGTH SOURCE LENGTH
/// NEXT-HOP …` in hexadecimal, whose next hop is unspecified, whose
/// length is at least `MIN_V6_PREFIX`, and which holds one of `own`:
/// a network the machine is on, and not a tunnel's route to the far
/// side or a route to nowhere.
fn networks_v6(routes: &str, own: &[(Ipv6Addr, u8)]) -> Vec<(Ipv6Addr, u8)> {
    let mut found = Vec::new();
    for line in routes.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let (Some(dest), Some(length), Some(hop)) = (
            words.first().and_then(|w| hex_v6(w)),
            words
                .get(1)
                .and_then(|w| u8::from_str_radix(w, 16).ok())
                .filter(|length| (MIN_V6_PREFIX..=128).contains(length)),
            words.get(4).and_then(|w| hex_v6(w)),
        ) else {
            continue;
        };
        let holds = own.iter().any(|(ip, _)| within_v6(*ip, dest, length));
        if hop.is_unspecified() && holds && !found.contains(&(dest, length)) {
            found.push((dest, length));
        }
    }
    found
}

/// An IPv6 address written as 32 hexadecimal digits.
fn hex_v6(hex: &str) -> Option<Ipv6Addr> {
    Some(hex)
        .filter(|hex| hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .and_then(|hex| u128::from_str_radix(hex, 16).ok())
        .map(Ipv6Addr::from)
}

fn reply(client: &mut UnixStream, said: &str) -> io::Result<()> {
    client.set_write_timeout(Some(HEAD_TIME))?;
    client.write_all(format!("{PROTOCOL}\n{said}\n\n").as_bytes())
}

/// The deadlines a relayed connection is held to.
#[derive(Clone, Copy, Debug)]
struct Deadlines {
    /// How long no byte may move either way.
    idle: Duration,
    /// How long it may last at all.
    total: Duration,
}

const DEADLINES: Deadlines = Deadlines {
    idle: IDLE,
    total: TOTAL,
};

/// A splice's clock, shared by its two directions: when it began, and
/// when a byte last moved either way.
struct Clock {
    began: Instant,
    /// Milliseconds after `began`.
    moved: AtomicU64,
    deadlines: Deadlines,
}

impl Clock {
    /// How long the next read or write may wait: until the idle deadline
    /// from the last byte moved, or the total, whichever is sooner;
    /// `None` once either has passed.
    fn wait(&self) -> Option<Duration> {
        let since = self.began.elapsed();
        let moved = Duration::from_millis(self.moved.load(Ordering::Acquire));
        let idle = moved.checked_add(self.deadlines.idle)?.checked_sub(since)?;
        let total = self.deadlines.total.checked_sub(since)?;
        Some(idle.min(total)).filter(|wait| !wait.is_zero())
    }

    fn moved(&self) {
        let since = u64::try_from(self.began.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.moved.fetch_max(since, Ordering::AcqRel);
    }
}

/// Copies bytes both ways until either side ends or fails, `idle` passes
/// with no byte moved either way, or `total` passes; then both ends of
/// both are shut, which wakes the other direction wherever it waits.
fn splice(client: UnixStream, origin: TcpStream, deadlines: Deadlines) {
    let clock = Arc::new(Clock {
        began: Instant::now(),
        moved: AtomicU64::new(0),
        deadlines,
    });
    let (Ok(client_back), Ok(origin_back)) = (client.try_clone(), origin.try_clone()) else {
        client.shut();
        origin.shut();
        return;
    };
    let up_clock = Arc::clone(&clock);
    let up = std::thread::Builder::new().spawn(move || {
        copy(client, origin, &up_clock);
    });
    copy(origin_back, client_back, &clock);
    if let Ok(up) = up {
        let _ = up.join();
    }
}

/// The two ends `copy` moves bytes between.
trait End: Read + Write {
    fn read_within(&self, wait: Duration) -> io::Result<()>;
    fn write_within(&self, wait: Duration) -> io::Result<()>;
    fn shut(&self);
}

impl End for UnixStream {
    fn read_within(&self, wait: Duration) -> io::Result<()> {
        self.set_read_timeout(Some(wait))
    }
    fn write_within(&self, wait: Duration) -> io::Result<()> {
        self.set_write_timeout(Some(wait))
    }
    fn shut(&self) {
        let _ = self.shutdown(Shutdown::Both);
    }
}

impl End for TcpStream {
    fn read_within(&self, wait: Duration) -> io::Result<()> {
        self.set_read_timeout(Some(wait))
    }
    fn write_within(&self, wait: Duration) -> io::Result<()> {
        self.set_write_timeout(Some(wait))
    }
    fn shut(&self) {
        let _ = self.shutdown(Shutdown::Both);
    }
}

/// Bytes from `from` to `to` until `from` ends or fails, a write to `to`
/// fails, or `clock` says a deadline passed: each read and each write
/// waits only as long as the clock allows, and a read or write that
/// times out looks at the clock again, since the other direction may
/// have moved bytes meanwhile. Then both ends are shut.
fn copy(mut from: impl End, mut to: impl End, clock: &Clock) {
    let mut buffer = vec![0u8; CHUNK];
    'reading: while let Some(wait) = clock.wait() {
        if from.read_within(wait).is_err() {
            break;
        }
        let read = match from.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(e) if waited(&e) => continue,
            Err(_) => break,
        };
        clock.moved();
        let mut rest = buffer.get(..read).unwrap_or_default();
        while !rest.is_empty() {
            let Some(wait) = clock.wait() else {
                break 'reading;
            };
            if to.write_within(wait).is_err() {
                break 'reading;
            }
            match to.write(rest) {
                Ok(0) => break 'reading,
                Ok(written) => {
                    clock.moved();
                    rest = rest.get(written..).unwrap_or_default();
                }
                Err(e) if waited(&e) => {}
                Err(_) => break 'reading,
            }
        }
    }
    from.shut();
    to.shut();
}

/// Whether `e` is a read or write that waited its time out, or was
/// interrupted, rather than failed.
fn waited(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

/// Asks the service at `socket` its probe, each read and write given
/// `budget`.
pub(crate) fn probe_within(socket: &Path, budget: Duration) -> Result<(), String> {
    let mut stream = UnixStream::connect(socket).map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(budget))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(budget))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(format!("{PROTOCOL}\nprobe\n\n").as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut said = String::new();
    stream
        .take(64)
        .read_to_string(&mut said)
        .map_err(|e| format!("read: {e}"))?;
    if said != format!("{PROTOCOL}\nok\n\n") {
        return Err(format!("answered {said:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPEN: Policy = Policy {
        allow_loopback: false,
    };

    fn none() -> Local {
        Local::default()
    }

    fn v4(text: &str) -> IpAddr {
        IpAddr::V4(text.parse().unwrap())
    }

    fn v6(text: &str) -> IpAddr {
        IpAddr::V6(text.parse().unwrap())
    }

    /// Every range §10 names is refused, in IPv4 and in each IPv6 form
    /// that carries an IPv4 address; public addresses, in every form, are
    /// not.
    #[test]
    fn the_predicate_refuses_every_range_and_every_carrying_form() {
        let refused_ones = [
            ("127.0.0.1", "loopback"),
            ("127.255.0.9", "loopback"),
            ("0.0.0.0", "this network"),
            ("0.1.2.3", "this network"),
            ("10.1.2.3", "RFC 1918"),
            ("172.16.0.1", "RFC 1918"),
            ("172.31.255.255", "RFC 1918"),
            ("192.168.1.1", "RFC 1918"),
            ("100.64.0.1", "carrier-grade"),
            ("100.127.255.255", "carrier-grade"),
            ("169.254.169.254", "link-local"),
            ("192.0.0.1", "192.0.0.0/24"),
            ("198.18.0.1", "198.18.0.0/15"),
            ("198.19.255.255", "198.18.0.0/15"),
            ("168.63.129.16", "platform"),
            ("224.0.0.1", "multicast"),
            ("240.0.0.1", "reserved"),
            ("255.255.255.255", "broadcast"),
        ];
        for (text, why) in refused_ones {
            let ip: Ipv4Addr = text.parse().unwrap();
            let said = refused(IpAddr::V4(ip), &none(), OPEN).unwrap_or_else(|| panic!("{text}"));
            assert!(said.contains(why), "{text}: {said}");
            // Each IPv6 form that carries it.
            let [a, b, c, d] = ip.octets();
            let (hi, lo) = (u16::from_be_bytes([a, b]), u16::from_be_bytes([c, d]));
            let forms = [
                Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, hi, lo),
                Ipv6Addr::new(0, 0, 0, 0, 0xffff, 0, hi, lo),
                Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, hi, lo),
                Ipv6Addr::new(0x2002, hi, lo, 0, 0, 0, 0, 1),
                Ipv6Addr::new(0x2001, 0, hi, lo, 0, 0, 0, 1),
                Ipv6Addr::new(0x2001, 0, 0x0808, 0x0808, 0, 0, !hi, !lo),
            ];
            for form in forms {
                assert!(
                    refused(IpAddr::V6(form), &none(), OPEN).is_some(),
                    "{form} carrying {text}"
                );
            }
            // IPv4-compatible, past the two addresses it overlaps.
            if ip.octets() != [0, 0, 0, 0] && ip.octets() != [0, 0, 0, 1] {
                let compatible = Ipv6Addr::new(0, 0, 0, 0, 0, 0, hi, lo);
                assert!(
                    refused(IpAddr::V6(compatible), &none(), OPEN).is_some(),
                    "{compatible}"
                );
            }
        }
        for (text, why) in [
            ("::", "unspecified"),
            ("::1", "loopback"),
            ("ff02::1", "multicast"),
            ("fe80::1", "link-local"),
            ("febf::1", "link-local"),
            ("fec0::1", "site-local"),
            ("fc00::1", "unique-local"),
            ("fdff::1", "unique-local"),
            ("64:ff9b:1::1", "local-use NAT64"),
        ] {
            let said = refused(v6(text), &none(), OPEN).unwrap_or_else(|| panic!("{text}"));
            assert!(said.contains(why), "{text}: {said}");
        }
        for public in [
            "1.1.1.1",
            "8.8.8.8",
            "172.15.255.255",
            "172.32.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "192.169.0.1",
            "192.0.1.1",
            "198.17.255.255",
            "198.20.0.1",
            "168.63.129.17",
            "223.255.255.255",
        ] {
            assert_eq!(refused(v4(public), &none(), OPEN), None, "{public}");
        }
        for public in [
            "2606:4700::1111",
            "::ffff:1.1.1.1",
            "::ffff:0:1.1.1.1",
            "64:ff9b::808:808",
            "2002:808:808::1",
            "2001:0:808:808::f7f7:f7f7",
        ] {
            assert_eq!(refused(v6(public), &none(), OPEN), None, "{public}");
        }
        // Loopback for the tests alone.
        let tests = Policy {
            allow_loopback: true,
        };
        assert_eq!(refused(v4("127.0.0.1"), &none(), tests), None);
        assert_eq!(refused(v6("::1"), &none(), tests), None);
        assert!(refused(v4("10.0.0.1"), &none(), tests).is_some());
    }

    /// This machine's own addresses and networks are refused, however
    /// carried, to the prefix's last address and no further.
    #[test]
    fn the_machines_own_addresses_and_networks_are_refused() {
        let local = Local {
            v4: vec![
                ("203.0.113.7".parse().unwrap(), 32),
                ("198.51.100.0".parse().unwrap(), 24),
            ],
            v6: vec![
                ("2001:db8::7".parse().unwrap(), 128),
                ("2001:db8:1::".parse().unwrap(), 64),
            ],
        };
        for ip in [
            v4("203.0.113.7"),
            v6("::ffff:203.0.113.7"),
            v6("2002:cb00:7107::1"),
            v4("198.51.100.0"),
            v4("198.51.100.255"),
            v6("2001:db8::7"),
            v6("2001:db8:1::1"),
            v6("2001:db8:1:0:ffff:ffff:ffff:ffff"),
        ] {
            let said = refused(ip, &local, OPEN).unwrap_or_else(|| panic!("{ip}"));
            assert!(said.contains("own addresses or networks"), "{ip}: {said}");
        }
        for ip in [
            v4("203.0.113.8"),
            v4("198.51.101.0"),
            v6("2001:db8::8"),
            v6("2001:db8:2::1"),
        ] {
            assert_eq!(refused(ip, &local, OPEN), None, "{ip}");
        }
    }

    /// The kernel's lists of this machine's addresses and networks are
    /// read as the kernel writes them.
    #[test]
    fn local_networks_are_read_from_the_kernels_lists() {
        let trie = "Main:\n  +-- 0.0.0.0/0 3 0 5\n     |-- 0.0.0.0\n        /0 universe UNICAST\n     |-- 0.0.0.0\n        /1 link UNICAST\n     |-- 192.0.2.0\n        /24 link UNICAST\n     |-- 10.9.0.0\n        /16 universe UNICAST\n     |-- 172.20.0.0\n        /16 link UNICAST tos=4\n     |-- 173.0.0.0\n        /8 link UNICAST\nLocal:\n  +-- 0.0.0.0/0 3 0 5\n     +-- 127.0.0.0/8 2 0 2\n        +-- 127.0.0.0/31 1 0 0\n           |-- 127.0.0.0\n              /8 host LOCAL\n           |-- 127.0.0.1\n              /32 host LOCAL\n        |-- 127.255.255.255\n           /32 link BROADCAST\n     |-- 192.0.2.10\n        /32 host LOCAL\n     |-- 192.0.2.10\n        /32 host LOCAL\n     |-- 198.51.100.0\n        /24 host LOCAL\n     |-- 172.20.0.5\n        /32 host LOCAL tos=4\n";
        assert_eq!(
            networks_v4(trie),
            [
                ("127.0.0.0".parse().unwrap(), 8),
                ("127.0.0.1".parse().unwrap(), 32),
                ("192.0.2.10".parse().unwrap(), 32),
                ("198.51.100.0".parse().unwrap(), 24),
                ("172.20.0.5".parse().unwrap(), 32),
                ("192.0.2.0".parse().unwrap(), 24),
                ("172.20.0.0".parse().unwrap(), 16),
            ]
        );
        let listed = "00000000000000000000000000000001 01 80 10 80       lo\nfe800000000000000000000000000001 02 40 20 80     eth0\n20010db8000000000000000000000007 02 40 00 80     eth0\nnot an address\n";
        assert_eq!(
            addresses_v6(listed),
            [
                ("::1".parse().unwrap(), 128),
                ("fe80::1".parse().unwrap(), 128),
                ("2001:db8::7".parse().unwrap(), 128),
            ]
        );
        let zero = "00000000000000000000000000000000";
        let hop = "fe800000000000000000000000000001";
        let routes = [
            format!("20010db8000000010000000000000000 40 {zero} 00 {zero} 00000100 00000001 00000000 00000001     eth0"),
            format!("{zero} 00 {zero} 00 {hop} 00000400 00000001 00000000 00000003     eth0"),
            format!("20000000000000000000000000000000 03 {zero} 00 {zero} 00000400 00000001 00000000 00000001      wg0"),
            format!("20010db8000000020000000000000000 40 {zero} 00 {hop} 00000400 00000001 00000000 00000003     eth0"),
            format!("20010db8000000000000000000000007 80 {zero} 00 {zero} 00000000 00000002 00000000 80200001     eth0"),
            format!("2a000000000000000000000000000000 10 {zero} 00 {zero} 00000400 00000001 00000000 00000001      wg0"),
            format!("2a010000000000000000000000000000 20 {zero} 00 {zero} 00000400 00000001 00000000 00200200       lo"),
            "short line".to_string(),
        ]
        .join("\n");
        let own = [
            ("2001:db8:0:1::5".parse().unwrap(), 128),
            ("2001:db8::7".parse().unwrap(), 128),
        ];
        assert_eq!(
            networks_v6(&routes, &own),
            [
                ("2001:db8:0:1::".parse().unwrap(), 64),
                ("2001:db8::7".parse().unwrap(), 128),
            ]
        );
        // This machine's, read now: loopback among them.
        let local = Local::read().unwrap();
        assert!(local.holds_v4("127.0.0.1".parse().unwrap()), "{local:?}");
    }

    /// A head is the protocol line and one probe or connect; a host is an
    /// address or a DNS name, a port 1 to 65535 in digits.
    #[test]
    fn heads_and_hosts_are_parsed_strictly() {
        let head =
            |lines: &[&str]| parse_head(&lines.iter().map(|l| l.to_string()).collect::<Vec<_>>());
        assert_eq!(head(&[PROTOCOL, "probe"]), Ok(Asked::Probe));
        assert_eq!(
            head(&[PROTOCOL, "connect Static.Crates.io 443"]),
            Ok(Asked::Connect(Host::Name("static.crates.io".into()), 443))
        );
        assert_eq!(
            head(&[PROTOCOL, "connect [2606:4700::1111] 443"]),
            Ok(Asked::Connect(Host::Address(v6("2606:4700::1111")), 443))
        );
        assert_eq!(
            head(&[PROTOCOL, "connect 1.1.1.1 80"]),
            Ok(Asked::Connect(Host::Address(v4("1.1.1.1")), 80))
        );
        for bad in [
            &[PROTOCOL][..],
            &["td-egress 2", "probe"],
            &[PROTOCOL, "connect h 0"],
            &[PROTOCOL, "connect h 65536"],
            &[PROTOCOL, "connect h +443"],
            &[PROTOCOL, "connect h 000443"],
            &[PROTOCOL, "connect h"],
            &[PROTOCOL, "connect -h 443"],
            &[PROTOCOL, "connect a..b 443"],
            &[PROTOCOL, "connect a_b 443"],
            &[PROTOCOL, "connect a/b 443"],
            &[PROTOCOL, "connect [1.1.1.1] 443"],
            &[PROTOCOL, "connect [::1 443"],
            &[PROTOCOL, "get h 443"],
        ] {
            assert!(head(bad).is_err(), "{bad:?}");
        }
        assert!(parse_host(&"a".repeat(64)).is_err());
        assert!(parse_host(&format!("{}.com", "a.".repeat(126))).is_err());
    }

    fn served(policy: Policy, deadlines: Deadlines) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "td-egress-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("td-egress").join("socket");
        let listener = crate::fetchd::bind(&socket).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || handle_within(stream, policy, deadlines));
            }
        });
        (dir, socket)
    }

    static NEXT: AtomicU64 = AtomicU64::new(0);

    const TESTS: Policy = Policy {
        allow_loopback: true,
    };

    fn ask(socket: &Path, request: &str) -> (UnixStream, String) {
        let mut stream = UnixStream::connect(socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut said = Vec::new();
        let mut byte = [0u8; 1];
        while !said.ends_with(b"\n\n") {
            if stream.read(&mut byte).unwrap() == 0 {
                break;
            }
            said.push(byte[0]);
        }
        (stream, String::from_utf8(said).unwrap())
    }

    /// A connection through a relay of `deadlines` to an origin on
    /// loopback that runs `origin` with its accepted connection.
    fn relayed(
        deadlines: Deadlines,
        origin: impl FnOnce(TcpStream) + Send + 'static,
    ) -> (std::path::PathBuf, UnixStream) {
        let (dir, socket) = served(TESTS, deadlines);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || origin(listener.accept().unwrap().0));
        let (stream, said) = ask(
            &socket,
            &format!("{PROTOCOL}\nconnect 127.0.0.1 {port}\n\n"),
        );
        assert_eq!(said, format!("{PROTOCOL}\nok\n\n"));
        (dir, stream)
    }

    /// The service answers its probe, relays a connection to an admitted
    /// destination both ways and ends it when the destination does, and
    /// refuses a destination no workspace may reach before connecting.
    #[test]
    fn a_connection_is_relayed_both_ways_and_a_refused_one_reaches_nothing() {
        let (dir, socket) = served(TESTS, DEADLINES);
        probe_within(&socket, Duration::from_secs(5)).unwrap();
        let (relay_dir, mut relayed) = relayed(DEADLINES, |mut origin| {
            let mut got = [0u8; 5];
            origin.read_exact(&mut got).unwrap();
            origin.write_all(&got).unwrap();
            origin.write_all(b" back").unwrap();
        });
        relayed.write_all(b"hello").unwrap();
        let mut back = String::new();
        relayed.read_to_string(&mut back).unwrap();
        assert_eq!(back, "hello back");
        // Refused before any connection: private, and a malformed head.
        for (request, answer) in [
            (
                format!("{PROTOCOL}\nconnect 10.0.0.1 443\n\n"),
                "error refused: 10.0.0.1 is ",
            ),
            (
                format!("{PROTOCOL}\nconnect [::ffff:192.168.0.1] 443\n\n"),
                "error refused: ::ffff:192.168.0.1 is ",
            ),
            (format!("{PROTOCOL}\nconnect h\n\n"), "error malformed:"),
        ] {
            let (_, said) = ask(&socket, &request);
            assert!(
                said.starts_with(&format!("{PROTOCOL}\n{answer}")),
                "{request:?}: {said:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&relay_dir);
    }

    /// Without the tests' policy, loopback is refused too, by name and by
    /// address.
    #[test]
    fn loopback_is_refused_but_for_the_tests() {
        let (dir, socket) = served(OPEN, DEADLINES);
        for host in ["127.0.0.1", "[::1]", "localhost"] {
            let (_, said) = ask(&socket, &format!("{PROTOCOL}\nconnect {host} 443\n\n"));
            assert!(
                said.starts_with(&format!("{PROTOCOL}\nerror refused:")),
                "{host}: {said:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    const SHORT: Deadlines = Deadlines {
        idle: Duration::from_millis(400),
        total: Duration::from_secs(20),
    };

    /// Bytes moving one way keep the connection: an origin that sends
    /// for longer than the idle deadline, to a client that sends nothing,
    /// is relayed whole.
    #[test]
    fn bytes_one_way_keep_the_connection_past_the_idle_deadline() {
        let (dir, mut client) = relayed(SHORT, |mut origin| {
            for _ in 0..12 {
                origin.write_all(b"x").unwrap();
                std::thread::sleep(Duration::from_millis(150));
            }
        });
        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"xxxxxxxxxxxx");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No byte either way for the idle deadline ends the connection, at
    /// both ends.
    #[test]
    fn a_silent_connection_ends_at_the_idle_deadline() {
        let (tell, told) = mpsc::channel();
        let began = Instant::now();
        let (dir, mut client) = relayed(SHORT, move |mut origin| {
            let mut got = Vec::new();
            let _ = origin.read_to_end(&mut got);
            let _ = tell.send(began.elapsed());
        });
        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        assert!(got.is_empty());
        let ended = told.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(ended < Duration::from_secs(5), "{ended:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An origin that stops reading cannot hold the relay: the write to
    /// it waits no longer than the idle deadline, and then the client's
    /// end is shut.
    #[test]
    fn an_origin_that_never_reads_is_let_go_at_the_idle_deadline() {
        let (dir, client) = relayed(SHORT, |origin| {
            std::thread::sleep(Duration::from_secs(30));
            drop(origin);
        });
        let began = Instant::now();
        let mut writer = client.try_clone().unwrap();
        let writing = std::thread::spawn(move || {
            let chunk = vec![0u8; 64 * 1024];
            while writer.write_all(&chunk).is_ok() {}
        });
        let mut reader = client;
        let mut got = Vec::new();
        let _ = reader.read_to_end(&mut got);
        writing.join().unwrap();
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "{:?}",
            began.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The client's end ends the origin's at once, though the origin is
    /// silent and the idle deadline far off.
    #[test]
    fn the_clients_end_ends_the_origins_at_once() {
        let (tell, told) = mpsc::channel();
        let began = Instant::now();
        let (dir, client) = relayed(DEADLINES, move |mut origin| {
            let mut got = Vec::new();
            let _ = origin.read_to_end(&mut got);
            let _ = tell.send(began.elapsed());
        });
        drop(client);
        let ended = told.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(ended < Duration::from_secs(5), "{ended:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bytes moving do not keep a connection past its total.
    #[test]
    fn a_busy_connection_ends_at_its_total() {
        let deadlines = Deadlines {
            idle: Duration::from_secs(20),
            total: Duration::from_millis(600),
        };
        let (dir, mut client) = relayed(deadlines, |mut origin| {
            while origin.write_all(b"x").is_ok() {
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let began = Instant::now();
        let mut got = Vec::new();
        let _ = client.read_to_end(&mut got);
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "{:?}",
            began.elapsed()
        );
        assert!(!got.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
