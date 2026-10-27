//! Wi-Fi for `td-netd watch`: the WPA2-Personal (or open) networks the owner
//! has joined, the one `join` named last that is in range first, each joined
//! over nl80211 with td-netd as its own supplicant (`wpa.rs`). Ethernet keeps
//! precedence: a new association starts only while no wired lease is held,
//! and `watch` gives up a radio lease when a cable gains carrier. Once the
//! station is authorized `watch` treats the radio as a link with carrier
//! and runs the same DHCP, renewal and expiry over it.
//!
//! EAPOL rides the nl80211 socket (control port over nl80211), so the
//! association belongs to this process's socket (SOCKET_OWNER): the kernel
//! drops it when the socket closes, and `watch` restarting reassociates.
//!
//! The session owner joins, lists and forgets networks through
//! `/run/td-netd/wifi`, a socket authenticated by its mode alone (0600, owned
//! by the account, in a root directory), as td-fetchd's is. Each network's
//! PMK, never its passphrase, is stored root-only in `/var/lib/td-netd/wifi`,
//! one line per network, in the order `join` last named them.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::genl::{self, Builder, NLMSG_DONE, NLMSG_ERROR, NLM_F_ACK, NLM_F_DUMP, NLM_F_REQUEST};
use crate::sys::{self, GenlSocket};
use crate::wpa;

const CONFIG_DIR: &str = "/var/lib/td-netd";
const CONFIG_PATH: &str = "/var/lib/td-netd/wifi";
const CONTROL_DIR: &str = "/run/td-netd";
pub const CONTROL_PATH: &str = "/run/td-netd/wifi";
/// The largest control request: `join`, a 32-byte SSID and a PMK in hex.
const MAX_REQUEST: u64 = 256;
/// Datagrams from nl80211 are at most a page or two; a scan dump packs
/// entries into them.
const RECV_BUFFER: usize = 64 * 1024;
const O_NOFOLLOW: i32 = 0o400000;
/// How often a frame the supplicant ignored may reach the log.
const IGNORED_LOG_INTERVAL: Duration = Duration::from_secs(10);
const SCAN_DEADLINE: Duration = Duration::from_secs(25);
/// How many networks are kept; joining another drops the least recent.
pub const MAX_NETWORKS: usize = 16;
/// The stored file's bound: `MAX_NETWORKS` lines of at most 130 bytes.
const MAX_CONFIG: u64 = 4096;
/// SSIDs one scan probes by name, the wildcard included, until the driver
/// refuses that many.
const PROBE_LIMIT: usize = 4;

// ── The saved networks ──────────────────────────────────────────────────────

#[derive(Clone, PartialEq, Eq)]
pub struct Network {
    pub ssid: Vec<u8>,
    /// WPA2's PMK; `None` for an open network.
    pub pmk: Option<[u8; 32]>,
}

impl Drop for Network {
    fn drop(&mut self) {
        if let Some(pmk) = &mut self.pmk {
            pmk.fill(0);
            std::hint::black_box(pmk);
        }
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    // `from_str_radix` alone would take a sign.
    if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

/// The request line, and the stored file, naming a network.
pub fn encode_network(network: &Network) -> String {
    let secret = network
        .pmk
        .map_or_else(|| "open".to_string(), |pmk| to_hex(&pmk));
    format!("{} {secret}", to_hex(&network.ssid))
}

pub fn decode_network(text: &str) -> Option<Network> {
    let mut fields = text.split_ascii_whitespace();
    let ssid = from_hex(fields.next()?)?;
    let secret = fields.next()?;
    if fields.next().is_some() || ssid.is_empty() || ssid.len() > 32 {
        return None;
    }
    let pmk = if secret == "open" {
        None
    } else {
        Some(<[u8; 32]>::try_from(from_hex(secret)?).ok()?)
    };
    Some(Network { ssid, pmk })
}

/// The stored list, one `encode_network` line each, most recent first. A
/// line that does not decode, a repeated SSID and anything past
/// `MAX_NETWORKS` are dropped rather than costing the rest.
pub fn decode_networks(text: &str) -> Vec<Network> {
    let mut networks: Vec<Network> = Vec::new();
    for network in text.lines().filter_map(decode_network) {
        if networks.len() < MAX_NETWORKS && !networks.iter().any(|n| n.ssid == network.ssid) {
            networks.push(network);
        }
    }
    networks
}

fn load() -> Vec<Network> {
    let mut bytes = Vec::new();
    match fs::File::open(CONFIG_PATH).and_then(|f| f.take(MAX_CONFIG).read_to_end(&mut bytes)) {
        // Lossy, so a stray byte costs its own line and not the list.
        Ok(_) => decode_networks(&String::from_utf8_lossy(&bytes)),
        Err(_) => Vec::new(),
    }
}

fn save(networks: &[Saved]) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(CONFIG_DIR) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    fs::set_permissions(CONFIG_DIR, fs::Permissions::from_mode(0o700))?;
    if networks.is_empty() {
        return match fs::remove_file(CONFIG_PATH) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let temporary = format!("{CONFIG_PATH}.new");
    match fs::remove_file(&temporary) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW)
        .open(&temporary)?;
    for saved in networks {
        writeln!(file, "{}", encode_network(&saved.network))?;
    }
    file.sync_all()?;
    fs::rename(&temporary, CONFIG_PATH)
}

/// A saved network and how its recent attempts went: one that failed sits
/// out its backoff while another saved network in range is tried.
#[derive(Clone)]
struct Saved {
    network: Network,
    failures: u32,
    rest_until: Option<Instant>,
}

impl Saved {
    fn new(network: Network) -> Self {
        Self {
            network,
            failures: 0,
            rest_until: None,
        }
    }

    fn resting(&self, now: Instant) -> bool {
        self.rest_until.is_some_and(|until| now < until)
    }
}

/// An SSID for a status line or a terminal: printable ASCII as itself,
/// anything else (it comes off the air) escaped.
pub fn show_ssid(ssid: &[u8]) -> String {
    let mut out = String::from("\"");
    for byte in ssid {
        match byte {
            b'"' | b'\\' => out.push_str(&format!("\\{}", *byte as char)),
            0x20..=0x7e => out.push(*byte as char),
            _ => out.push_str(&format!("\\x{byte:02x}")),
        }
    }
    out.push('"');
    out
}

// ── nl80211 over one generic netlink socket ─────────────────────────────────

struct Nl80211 {
    socket: GenlSocket,
    family: u16,
    seq: u32,
    /// Messages that arrived while a request waited: multicast events and
    /// control port frames, as (command, attributes).
    events: VecDeque<(u8, Vec<u8>)>,
    buf: Vec<u8>,
}

impl Nl80211 {
    fn open() -> io::Result<Self> {
        let socket = GenlSocket::open()?;
        let mut this = Self {
            socket,
            family: genl::GENL_ID_CTRL,
            seq: 0,
            events: VecDeque::new(),
            buf: vec![0; RECV_BUFFER],
        };
        let seq = this.next_seq();
        let replies = this.transact(&genl::family_request(seq, "nl80211"), seq, false)?;
        let family = replies
            .iter()
            .find_map(|reply| genl::parse_family(genl::genl(reply)?.1))
            .ok_or_else(|| io::Error::other("nl80211 is not registered (no cfg80211)"))?;
        this.family = family.id;
        for group in ["mlme", "scan"] {
            let id = family
                .group(group)
                .ok_or_else(|| io::Error::other(format!("nl80211 has no {group} group")))?;
            this.socket.join_group(id)?;
        }
        this.events.clear();
        Ok(this)
    }

    fn next_seq(&mut self) -> u32 {
        self.seq = self.seq.wrapping_add(1).max(1);
        self.seq
    }

    /// One nl80211 command, acknowledged (or, for a dump, completed): its
    /// replies' generic netlink payloads.
    fn request(
        &mut self,
        cmd: u8,
        dump: bool,
        build: impl FnOnce(&mut Builder),
    ) -> io::Result<Vec<Vec<u8>>> {
        let seq = self.next_seq();
        let flags = NLM_F_REQUEST | if dump { NLM_F_DUMP } else { NLM_F_ACK };
        let mut builder = Builder::new(self.family, flags, seq, cmd);
        build(&mut builder);
        self.transact(&builder.finish(), seq, dump)
    }

    fn transact(&mut self, message: &[u8], seq: u32, dump: bool) -> io::Result<Vec<Vec<u8>>> {
        self.socket.send(message)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut replies = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "nl80211 did not answer",
                ));
            }
            let (ready, _) = sys::poll2(Some(self.socket.fd()), None, left)?;
            if !ready {
                continue;
            }
            while let Some(len) = self.socket.recv(&mut self.buf)? {
                let datagram = self.buf.get(..len).unwrap_or(&[]);
                let mut finished = None;
                for message in genl::messages(datagram) {
                    if message.seq != seq || message.seq == 0 {
                        if message.kind == self.family {
                            if let Some((cmd, attributes)) = genl::genl(message.payload) {
                                self.events.push_back((cmd, attributes.to_vec()));
                            }
                        }
                        continue;
                    }
                    match message.kind {
                        NLMSG_ERROR => {
                            let code = genl::error_code(message.payload).unwrap_or(-5);
                            finished = Some(if code == 0 {
                                Ok(())
                            } else {
                                Err(io::Error::from_raw_os_error(-code))
                            });
                        }
                        NLMSG_DONE if dump => finished = Some(Ok(())),
                        _ => replies.push(message.payload.to_vec()),
                    }
                }
                if let Some(result) = finished {
                    return result.map(|()| replies);
                }
            }
        }
    }

    /// Read whatever is queued on the socket into the event queue.
    fn drain(&mut self) -> io::Result<()> {
        while let Some(len) = self.socket.recv(&mut self.buf)? {
            let datagram = self.buf.get(..len).unwrap_or(&[]);
            for message in genl::messages(datagram) {
                if message.kind == self.family {
                    if let Some((cmd, attributes)) = genl::genl(message.payload) {
                        self.events.push_back((cmd, attributes.to_vec()));
                    }
                }
            }
        }
        Ok(())
    }
}

// ── The station ─────────────────────────────────────────────────────────────

struct Interface {
    name: String,
    index: u32,
    mac: [u8; 6],
}

enum Phase {
    /// No wireless interface yet, or it went away.
    Absent,
    /// Waiting to (re)associate.
    Idle {
        retry: Instant,
    },
    Scanning {
        deadline: Instant,
    },
    /// CONNECT sent; the association result, or EAPOL, follows.
    Joining {
        deadline: Instant,
        bss: [u8; 6],
        supplicant: Option<wpa::Supplicant>,
        associated: bool,
        failure: Option<&'static str>,
    },
    /// Authorized: `watch` may run DHCP over the link.
    Ready {
        bss: [u8; 6],
        supplicant: Option<wpa::Supplicant>,
    },
}

/// How `watch` hears what the radio is doing: a status line for the shared
/// status file (only while the radio is the link in question) and the log.
pub type Report<'a> = &'a mut dyn FnMut(&str);

pub struct Wifi {
    control: Option<UnixListener>,
    interface: Option<Interface>,
    nl: Option<Nl80211>,
    networks: Vec<Saved>,
    /// The network a `join` request named: the next attempt is its alone,
    /// so the owner's client hears about that network.
    pinned: Option<Vec<u8>>,
    /// The network being joined, or joined.
    current: Option<Vec<u8>>,
    /// SSIDs a scan may probe by name, the wildcard included.
    probe_limit: usize,
    phase: Phase,
    failures: u32,
    next_probe: Instant,
    last_status: String,
    /// A wired lease holds the link, as `drive` last heard.
    wired: bool,
    last_ignored: Option<Instant>,
}

fn backoff(failures: u32) -> Duration {
    Duration::from_secs(match failures {
        0 | 1 => 2,
        2 => 5,
        3 => 10,
        4 => 30,
        _ => 60,
    })
}

fn wireless_interface() -> Option<String> {
    let mut names: Vec<String> = fs::read_dir("/sys/class/net")
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry.path().join("wireless").exists() && entry.path().join("phy80211").exists()
        })
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .collect();
    names.sort();
    names.into_iter().next()
}

fn random_nonce() -> io::Result<[u8; 32]> {
    let mut nonce = [0u8; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut nonce)?;
    Ok(nonce)
}

/// Bind the owner's control socket: a root directory no one else can write
/// and a socket only the owner (and root) may connect to. The directory is
/// root's alone (0700) from before the bind until the socket is the
/// owner's at 0600, so no umask lets anyone else connect in between.
fn bind_control(owner: u32) -> io::Result<UnixListener> {
    match fs::symlink_metadata(CONTROL_DIR) {
        Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => {
            return Err(io::Error::other(format!(
                "{CONTROL_DIR} is not a directory"
            )));
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(CONTROL_DIR)?
        }
        Err(e) => return Err(e),
    }
    std::os::unix::fs::chown(CONTROL_DIR, Some(0), Some(0))?;
    fs::set_permissions(CONTROL_DIR, fs::Permissions::from_mode(0o700))?;
    match fs::remove_file(CONTROL_PATH) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let listener = UnixListener::bind(CONTROL_PATH)?;
    std::os::unix::fs::chown(CONTROL_PATH, Some(owner), None)?;
    fs::set_permissions(CONTROL_PATH, fs::Permissions::from_mode(0o600))?;
    fs::set_permissions(CONTROL_DIR, fs::Permissions::from_mode(0o755))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

impl Wifi {
    pub fn new(owner: Option<u32>, report: Report<'_>) -> Self {
        let control = owner.and_then(|uid| match bind_control(uid) {
            Ok(listener) => Some(listener),
            Err(e) => {
                report(&format!("wifi: control socket unavailable: {e}"));
                None
            }
        });
        Self {
            control,
            interface: None,
            nl: None,
            networks: load().into_iter().map(Saved::new).collect(),
            pinned: None,
            current: None,
            probe_limit: PROBE_LIMIT,
            phase: Phase::Absent,
            failures: 0,
            next_probe: Instant::now(),
            last_status: String::new(),
            wired: false,
            last_ignored: None,
        }
    }

    /// The wireless interface's name, once one is in use.
    pub fn interface(&self) -> Option<&str> {
        self.interface.as_ref().map(|i| i.name.as_str())
    }

    /// Whether the station is authorized, so DHCP may run over it.
    pub fn ready(&self) -> bool {
        matches!(self.phase, Phase::Ready { .. })
    }

    fn status(&mut self, report: Report<'_>, message: String) {
        if self.last_status != message {
            report(&message);
            self.last_status = message;
        }
    }

    fn name(&self) -> String {
        self.interface().unwrap_or("wifi").to_string()
    }

    fn ssid(&self) -> String {
        self.current
            .as_deref()
            .map_or_else(|| "\"\"".into(), show_ssid)
    }

    /// What a scan looks for, for its status line.
    fn wanted(&self) -> String {
        match (&self.pinned, self.networks.as_slice()) {
            (Some(ssid), _) => show_ssid(ssid),
            (None, [only]) => show_ssid(&only.network.ssid),
            (None, saved) => format!("{} saved networks", saved.len()),
        }
    }

    /// Give up the association and try again after the backoff. The
    /// network that failed rests past the next scan and then for its own
    /// backoff, and a pinned join gives way to every saved network.
    fn fail(&mut self, report: Report<'_>, message: String) {
        self.failures = self.failures.saturating_add(1);
        let wait = backoff(self.failures);
        if let Some(ssid) = self.current.take() {
            if let Some(saved) = self.networks.iter_mut().find(|s| s.network.ssid == ssid) {
                saved.failures = saved.failures.saturating_add(1);
                saved.rest_until =
                    Some(Instant::now() + wait + SCAN_DEADLINE + backoff(saved.failures));
            }
        }
        self.pinned = None;
        self.disconnect();
        self.phase = Phase::Idle {
            retry: Instant::now() + wait,
        };
        self.status(
            report,
            format!("{message}; retrying in {} seconds", wait.as_secs()),
        );
    }

    fn disconnect(&mut self) {
        let (Some(nl), Some(interface)) = (self.nl.as_mut(), self.interface.as_ref()) else {
            return;
        };
        let index = interface.index;
        // ENOTCONN when nothing is associated is the expected answer.
        let _ = nl.request(genl::CMD_DISCONNECT, false, |b| {
            b.u32(genl::ATTR_IFINDEX, index)
                .u16(genl::ATTR_REASON_CODE, 3);
        });
    }

    /// The owner chose or forgot a network: start over now.
    fn restart_now(&mut self) {
        if !matches!(self.phase, Phase::Absent) {
            self.disconnect();
            self.phase = Phase::Idle {
                retry: Instant::now(),
            };
        }
        self.failures = 0;
        self.current = None;
        // Whatever was pinned is not being followed any more; a followed
        // join pins again after this.
        self.pinned = None;
        self.last_status.clear();
    }

    /// A frame the supplicant ignored: the log only, and rarely, since
    /// anyone in radio range can send them.
    fn ignored(&mut self, why: &str) {
        let now = Instant::now();
        if self
            .last_ignored
            .is_some_and(|last| now.duration_since(last) < IGNORED_LOG_INTERVAL)
        {
            return;
        }
        self.last_ignored = Some(now);
        let _ = writeln!(
            io::stderr(),
            "td-netd: wifi: EAPOL-Key frame ignored: {why}"
        );
    }

    /// Advance timers: find the interface, start a scan or give up on a
    /// stalled association. `wired` holds new associations back.
    pub fn drive(&mut self, ctl: RawFd, wired: bool, report: Report<'_>) {
        let now = Instant::now();
        self.wired = wired;
        if let Some(interface) = &self.interface {
            if !Path::new("/sys/class/net").join(&interface.name).exists() {
                report(&format!("{}: wireless interface went away", interface.name));
                self.interface = None;
                self.phase = Phase::Absent;
            }
        }
        if self.interface.is_none() {
            if now < self.next_probe {
                return;
            }
            self.next_probe = now + Duration::from_secs(2);
            if let Err(e) = self.probe(ctl, report) {
                self.status(report, format!("wifi: {e}"));
            }
            return;
        }
        match &self.phase {
            Phase::Idle { retry } if now >= *retry && !wired => {
                self.current = None;
                if self.networks.is_empty() {
                    let message = format!(
                        "{}: disconnected: no Wi-Fi network chosen; run `td-netd wifi join NAME`",
                        self.name()
                    );
                    self.status(report, message);
                    self.phase = Phase::Idle {
                        retry: now + Duration::from_secs(5),
                    };
                    return;
                }
                if let Err(e) = self.scan() {
                    if e.raw_os_error() == Some(16) {
                        self.phase = Phase::Idle {
                            retry: now + Duration::from_secs(2),
                        };
                    } else {
                        let message = format!("{}: scan failed: {e}", self.name());
                        self.fail(report, message);
                    }
                    return;
                }
                let message = format!("{}: scanning for {}", self.name(), self.wanted());
                self.status(report, message);
                // A tri-band WCN6855 scan can run past ten seconds; the
                // scan's own completion event normally ends this earlier.
                self.phase = Phase::Scanning {
                    deadline: now + SCAN_DEADLINE,
                };
            }
            Phase::Scanning { deadline } if now >= *deadline => self.scanned(report),
            Phase::Joining {
                deadline,
                associated,
                failure,
                supplicant,
                ..
            } if now >= *deadline => {
                let refused = supplicant
                    .as_ref()
                    .is_some_and(wpa::Supplicant::awaiting_message_3);
                let why = match (associated, failure) {
                    (_, Some(why)) => {
                        format!("authentication failed ({why}); check the passphrase")
                    }
                    (true, None) if refused => WRONG_PASSPHRASE.into(),
                    (true, None) => "the access point did not complete the key handshake".into(),
                    (false, None) => "association timed out".into(),
                };
                let message = format!("{}: joining {} failed: {why}", self.name(), self.ssid());
                self.fail(report, message);
            }
            _ => {}
        }
    }

    fn probe(&mut self, ctl: RawFd, report: Report<'_>) -> io::Result<()> {
        let Some(name) = wireless_interface() else {
            return Ok(());
        };
        let index: u32 =
            fs::read_to_string(Path::new("/sys/class/net").join(&name).join("ifindex"))?
                .trim()
                .parse()
                .map_err(io::Error::other)?;
        if self.nl.is_none() {
            self.nl = Some(Nl80211::open()?);
        }
        // A previous watch's association died with its socket; its address
        // and routes must not outlive it. Taking the link down flushes them.
        crate::set_link_enabled(ctl, &name, false)?;
        crate::set_addr(ctl, &name, crate::SIOCSIFADDR, [0; 4])?;
        crate::set_link_up(ctl, &name)?;
        let mac = crate::get_mac(ctl, &name)?;
        report(&format!("{name}: wireless interface ready"));
        self.interface = Some(Interface { name, index, mac });
        self.probe_limit = PROBE_LIMIT;
        self.phase = Phase::Idle {
            retry: Instant::now(),
        };
        Ok(())
    }

    /// The SSIDs a scan probes by name, so hidden networks answer: the
    /// pinned one, or the most recent saved ones within the probe limit.
    fn probes(&self) -> Vec<Vec<u8>> {
        match &self.pinned {
            Some(ssid) if self.probe_limit > 1 => vec![ssid.clone()],
            Some(_) => Vec::new(),
            None => self
                .networks
                .iter()
                .take(self.probe_limit.saturating_sub(1))
                .map(|s| s.network.ssid.clone())
                .collect(),
        }
    }

    fn scan(&mut self) -> io::Result<()> {
        loop {
            let probes = self.probes();
            let (Some(nl), Some(interface)) = (self.nl.as_mut(), self.interface.as_ref()) else {
                return Err(io::Error::other("no wireless interface"));
            };
            let index = interface.index;
            let result = nl.request(genl::CMD_TRIGGER_SCAN, false, |b| {
                b.u32(genl::ATTR_IFINDEX, index)
                    .begin(genl::ATTR_SCAN_SSIDS);
                // The wildcard, last, finds everything else.
                let mut slot = 1;
                for ssid in &probes {
                    b.bytes(slot, ssid);
                    slot += 1;
                }
                b.bytes(slot, &[]).end();
            });
            match result {
                // EINVAL: more SSIDs than the wiphy probes in one scan; ask
                // for one fewer, down to the wildcard alone.
                Err(e) if e.raw_os_error() == Some(22) && !probes.is_empty() => {
                    self.probe_limit = probes.len();
                    let _ = writeln!(
                        io::stderr(),
                        "td-netd: wifi: the radio refused {} SSIDs in one scan; probing {} by name from now on",
                        probes.len() + 1,
                        probes.len() - 1
                    );
                }
                result => return result.map(|_| ()),
            }
        }
    }

    fn results(&mut self) -> io::Result<Vec<genl::Bss>> {
        let (Some(nl), Some(interface)) = (self.nl.as_mut(), self.interface.as_ref()) else {
            return Err(io::Error::other("no wireless interface"));
        };
        let index = interface.index;
        let replies = nl.request(genl::CMD_GET_SCAN, true, |b| {
            b.u32(genl::ATTR_IFINDEX, index);
        })?;
        Ok(replies
            .iter()
            .filter_map(|reply| genl::parse_bss(genl::genl(reply)?.1))
            .collect())
    }

    /// The scan finished (or its deadline passed): join the best BSS.
    fn scanned(&mut self, report: Report<'_>) {
        let bsses = match self.results() {
            Ok(bsses) => bsses,
            Err(e) => {
                let message = format!("{}: reading scan results failed: {e}", self.name());
                return self.fail(report, message);
            }
        };
        if self.networks.is_empty() {
            self.phase = Phase::Idle {
                retry: Instant::now(),
            };
            return;
        }
        let chosen = select(
            &bsses,
            &self.networks,
            self.pinned.as_deref(),
            Instant::now(),
        );
        let (network, bss) = match chosen {
            Ok((index, bss)) => match self.networks.get(index) {
                Some(saved) => (saved.network.clone(), bss.clone()),
                None => {
                    self.phase = Phase::Idle {
                        retry: Instant::now(),
                    };
                    return;
                }
            },
            Err(why) => {
                let message = format!("{}: {why}", self.name());
                return self.fail(report, message);
            }
        };
        self.current = Some(network.ssid.clone());
        if let Err(e) = self.connect(&network, &bss) {
            let message = format!("{}: joining {} failed: {e}", self.name(), self.ssid());
            return self.fail(report, message);
        }
        let message = format!(
            "{}: joining {} ({} at {} MHz, {} dBm)",
            self.name(),
            self.ssid(),
            mac_text(&bss.bssid),
            bss.frequency,
            bss.signal_mbm / 100
        );
        self.status(report, message);
    }

    fn connect(&mut self, network: &Network, bss: &genl::Bss) -> io::Result<()> {
        let (Some(nl), Some(interface)) = (self.nl.as_mut(), self.interface.as_ref()) else {
            return Err(io::Error::other("no wireless interface"));
        };
        let supplicant = match (network.pmk, wpa::security(&bss.ies)) {
            (Some(pmk), wpa::Security::Wpa2Psk(rsn)) => {
                Some(wpa::Supplicant::new(pmk, bss.bssid, interface.mac, rsn))
            }
            _ => None,
        };
        let index = interface.index;
        let protected = supplicant.is_some();
        let request = |b: &mut Builder| {
            b.u32(genl::ATTR_IFINDEX, index)
                .bytes(genl::ATTR_MAC, &bss.bssid)
                .u32(genl::ATTR_WIPHY_FREQ, bss.frequency)
                .bytes(genl::ATTR_SSID, &network.ssid)
                .u32(genl::ATTR_AUTH_TYPE, genl::AUTHTYPE_OPEN_SYSTEM)
                .flag(genl::ATTR_SOCKET_OWNER);
            if protected {
                b.flag(genl::ATTR_PRIVACY)
                    .u32(genl::ATTR_WPA_VERSIONS, genl::WPA_VERSION_2)
                    .bytes(
                        genl::ATTR_CIPHER_SUITES_PAIRWISE,
                        &wpa::CIPHER_CCMP.to_ne_bytes(),
                    )
                    .u32(genl::ATTR_CIPHER_SUITE_GROUP, wpa::CIPHER_CCMP)
                    .bytes(genl::ATTR_AKM_SUITES, &wpa::AKM_PSK.to_ne_bytes())
                    .bytes(genl::ATTR_IE, &wpa::STATION_RSN)
                    .u32(genl::ATTR_USE_MFP, genl::MFP_NO)
                    .flag(genl::ATTR_CONTROL_PORT)
                    .u16(genl::ATTR_CONTROL_PORT_ETHERTYPE, genl::ETH_P_PAE)
                    .flag(genl::ATTR_CONTROL_PORT_OVER_NL80211);
            }
        };
        match nl.request(genl::CMD_CONNECT, false, request) {
            // EALREADY: a stale association; drop it and let the retry run.
            Err(e) if e.raw_os_error() == Some(114) => {
                self.disconnect();
                Err(e)
            }
            Err(e) => Err(e),
            Ok(_) => {
                self.phase = Phase::Joining {
                    deadline: Instant::now() + Duration::from_secs(15),
                    bss: bss.bssid,
                    supplicant,
                    associated: false,
                    failure: None,
                };
                Ok(())
            }
        }
    }

    /// Wait up to `timeout` for nl80211 events and control requests, and
    /// handle them.
    pub fn wait(&mut self, timeout: Duration, report: Report<'_>) {
        let queued = self.nl.as_ref().is_some_and(|nl| !nl.events.is_empty());
        let nl_fd = self.nl.as_ref().map(|nl| nl.socket.fd());
        let control_fd = self.control.as_ref().map(|l| l.as_raw_fd());
        let timeout = if queued { Duration::ZERO } else { timeout };
        let (nl_ready, control_ready) = match sys::poll2(nl_fd, control_fd, timeout) {
            Ok(ready) => ready,
            Err(e) => {
                report(&format!("wifi: poll failed: {e}"));
                std::thread::sleep(timeout);
                (false, false)
            }
        };
        if nl_ready {
            if let Some(nl) = self.nl.as_mut() {
                if let Err(e) = nl.drain() {
                    report(&format!("wifi: nl80211 receive failed: {e}"));
                }
            }
        }
        while let Some((cmd, attributes)) = self.nl.as_mut().and_then(|nl| nl.events.pop_front()) {
            self.event(cmd, &attributes, report);
        }
        if control_ready {
            self.accept(report);
        }
    }

    fn event(&mut self, cmd: u8, attributes: &[u8], report: Report<'_>) {
        let Some(index) = self.interface.as_ref().map(|i| i.index) else {
            return;
        };
        if genl::attr_u32(attributes, genl::ATTR_IFINDEX) != Some(index) {
            return;
        }
        match cmd {
            genl::CMD_NEW_SCAN_RESULTS | genl::CMD_SCAN_ABORTED => {
                if matches!(self.phase, Phase::Scanning { .. }) {
                    self.scanned(report);
                }
            }
            genl::CMD_CONNECT => {
                let status = genl::attr_u16(attributes, genl::ATTR_STATUS_CODE);
                let timed_out = genl::attr(attributes, genl::ATTR_TIMED_OUT).is_some();
                let Phase::Joining {
                    associated,
                    supplicant,
                    bss,
                    ..
                } = &mut self.phase
                else {
                    return;
                };
                if status != Some(0) || timed_out {
                    let why = if timed_out {
                        "no answer".to_string()
                    } else {
                        format!("status {}", status.unwrap_or(1))
                    };
                    let message = format!(
                        "{}: association with {} refused: {why}",
                        self.name(),
                        self.ssid()
                    );
                    return self.fail(report, message);
                }
                *associated = true;
                if supplicant.is_none() {
                    let bss = *bss;
                    self.authorized(bss, None, report);
                }
            }
            genl::CMD_DISCONNECT | genl::CMD_DEAUTHENTICATE | genl::CMD_DISASSOCIATE => {
                let refused = matches!(
                    &self.phase,
                    Phase::Joining { supplicant: Some(s), .. } if s.awaiting_message_3()
                );
                if refused {
                    let message = format!(
                        "{}: joining {} failed: {WRONG_PASSPHRASE}",
                        self.name(),
                        self.ssid()
                    );
                    self.fail(report, message);
                } else if matches!(self.phase, Phase::Joining { .. } | Phase::Ready { .. }) {
                    let reason = genl::attr_u16(attributes, genl::ATTR_REASON_CODE).unwrap_or(0);
                    let message = format!(
                        "{}: disconnected from {} (reason {reason})",
                        self.name(),
                        self.ssid()
                    );
                    self.fail(report, message);
                }
            }
            genl::CMD_CONTROL_PORT_FRAME => self.eapol(attributes, report),
            _ => {}
        }
    }

    fn eapol(&mut self, attributes: &[u8], report: Report<'_>) {
        let (Some(frame), Some(source)) = (
            genl::attr(attributes, genl::ATTR_FRAME),
            genl::attr(attributes, genl::ATTR_MAC),
        ) else {
            return;
        };
        if genl::attr_u16(attributes, genl::ATTR_CONTROL_PORT_ETHERTYPE) != Some(genl::ETH_P_PAE) {
            return;
        }
        let encrypted = genl::attr(attributes, genl::ATTR_CONTROL_PORT_NO_ENCRYPT).is_none();
        let nonce = match random_nonce() {
            Ok(nonce) => nonce,
            Err(e) => return report(&format!("wifi: no randomness for the handshake: {e}")),
        };
        let (bss, supplicant, failure) = match &mut self.phase {
            Phase::Joining {
                bss,
                supplicant: Some(supplicant),
                failure,
                ..
            } => (*bss, supplicant, Some(failure)),
            Phase::Ready {
                bss,
                supplicant: Some(supplicant),
            } => (*bss, supplicant, None),
            _ => return,
        };
        if source != bss {
            return;
        }
        let actions = match supplicant.receive(frame, encrypted, nonce) {
            Ok(actions) => actions,
            Err(why) => {
                // Only a MIC checked under this PMK speaks for the
                // passphrase; anyone in range can send the rest.
                if why == "message 3 MIC mismatch" {
                    if let Some(failure) = failure {
                        *failure = Some(why);
                    }
                }
                return self.ignored(why);
            }
        };
        let mut authorize = false;
        let mut failed = None;
        for action in actions {
            if failed.is_some() {
                forget(action);
            } else if let Err(e) = self.act(&bss, action, &mut authorize) {
                failed = Some(e);
            }
        }
        if let Some(e) = failed {
            let message = format!(
                "{}: installing keys for {} failed: {e}",
                self.name(),
                self.ssid()
            );
            return self.fail(report, message);
        }
        if authorize {
            let phase = std::mem::replace(&mut self.phase, Phase::Absent);
            if let Phase::Joining { supplicant, .. } = phase {
                self.authorized(bss, supplicant, report);
            } else {
                self.phase = phase;
            }
        }
    }

    fn act(&mut self, bss: &[u8; 6], action: wpa::Action, authorize: &mut bool) -> io::Result<()> {
        let (Some(nl), Some(interface)) = (self.nl.as_mut(), self.interface.as_ref()) else {
            return Err(io::Error::other("no wireless interface"));
        };
        let index = interface.index;
        match action {
            wpa::Action::Send { frame, encrypt } => nl
                .request(genl::CMD_CONTROL_PORT_FRAME, false, |b| {
                    b.u32(genl::ATTR_IFINDEX, index)
                        .bytes(genl::ATTR_FRAME, &frame)
                        .bytes(genl::ATTR_MAC, bss)
                        .u16(genl::ATTR_CONTROL_PORT_ETHERTYPE, genl::ETH_P_PAE);
                    if !encrypt {
                        b.flag(genl::ATTR_CONTROL_PORT_NO_ENCRYPT);
                    }
                })
                .map(|_| ()),
            wpa::Action::PairwiseKey { mut tk } => {
                let result = nl.request(genl::CMD_NEW_KEY, false, |b| {
                    b.u32(genl::ATTR_IFINDEX, index)
                        .bytes(genl::ATTR_MAC, bss)
                        .begin(genl::ATTR_KEY)
                        .bytes(genl::KEY_DATA, &tk)
                        .u8(genl::KEY_IDX, 0)
                        .u32(genl::KEY_CIPHER, wpa::CIPHER_CCMP)
                        .u32(genl::KEY_TYPE, genl::KEYTYPE_PAIRWISE)
                        .end();
                });
                tk.fill(0);
                std::hint::black_box(&mut tk);
                result.map(|_| ())
            }
            wpa::Action::GroupKey {
                index: key_index,
                mut key,
                rsc,
            } => {
                let result = nl.request(genl::CMD_NEW_KEY, false, |b| {
                    b.u32(genl::ATTR_IFINDEX, index)
                        .begin(genl::ATTR_KEY)
                        .bytes(genl::KEY_DATA, &key)
                        .u8(genl::KEY_IDX, key_index)
                        .u32(genl::KEY_CIPHER, wpa::CIPHER_CCMP)
                        .bytes(genl::KEY_SEQ, &rsc)
                        .u32(genl::KEY_TYPE, genl::KEYTYPE_GROUP)
                        .end();
                });
                key.fill(0);
                std::hint::black_box(&mut key);
                result.map(|_| ())
            }
            wpa::Action::Authorize => {
                // struct nl80211_sta_flag_update: the mask, then the set.
                let bit = (1u32 << genl::STA_FLAG_AUTHORIZED).to_ne_bytes();
                let flags: Vec<u8> = bit.iter().chain(&bit).copied().collect();
                nl.request(genl::CMD_SET_STATION, false, |b| {
                    b.u32(genl::ATTR_IFINDEX, index)
                        .bytes(genl::ATTR_MAC, bss)
                        .bytes(genl::ATTR_STA_FLAGS2, &flags);
                })?;
                *authorize = true;
                Ok(())
            }
        }
    }

    fn authorized(
        &mut self,
        bss: [u8; 6],
        supplicant: Option<wpa::Supplicant>,
        report: Report<'_>,
    ) {
        self.failures = 0;
        self.pinned = None;
        if let Some(ssid) = &self.current {
            if let Some(saved) = self.networks.iter_mut().find(|s| &s.network.ssid == ssid) {
                saved.failures = 0;
                saved.rest_until = None;
            }
        }
        self.phase = Phase::Ready { bss, supplicant };
        let message = format!(
            "{}: joined {}; requesting DHCP lease",
            self.name(),
            self.ssid()
        );
        self.status(report, message);
    }

    // ── The owner's control socket ──────────────────────────────────────────

    fn accept(&mut self, report: Report<'_>) {
        let Some(listener) = self.control.as_ref() else {
            return;
        };
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
            Err(e) => return report(&format!("wifi: control accept failed: {e}")),
        };
        let mut reply = Vec::new();
        if let Err(e) = self.serve(&stream, &mut reply, report) {
            reply = format!("error {e}\n").into_bytes();
        }
        let _ = (&stream).write_all(&reply);
    }

    fn serve(
        &mut self,
        stream: &UnixStream,
        reply: &mut Vec<u8>,
        report: Report<'_>,
    ) -> io::Result<()> {
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut line = String::new();
        io::BufReader::new(stream.take(MAX_REQUEST)).read_line(&mut line)?;
        let line = line.trim_end_matches('\n');
        match parse_request(line)? {
            Request::Join(network) => {
                let ssid = network.ssid.clone();
                let networks = joined(&self.networks, network);
                save(&networks)?;
                self.networks = networks;
                self.restart_now();
                // The reply names the interface whose status lines answer
                // the join; that status is written before the reply, so the
                // client never reads a line from an earlier attempt. Only a
                // join the client follows is pinned: a saved one is first
                // in the list anyway.
                match self.interface().map(str::to_owned) {
                    Some(_) if self.wired => writeln!(reply, "ok wired")?,
                    Some(name) => {
                        self.pinned = Some(ssid.clone());
                        self.current = Some(ssid);
                        let message = format!("{name}: joining {}", self.ssid());
                        self.status(report, message);
                        writeln!(reply, "ok {name}")?;
                    }
                    None => writeln!(reply, "ok")?,
                }
            }
            Request::Forget(ssid) => {
                let networks = forgotten(&self.networks, ssid.as_deref())?;
                save(&networks)?;
                self.networks = networks;
                let in_use = |held: &Option<Vec<u8>>| match (&ssid, held) {
                    (None, _) => true,
                    (Some(ssid), Some(held)) => ssid == held,
                    (Some(_), None) => false,
                };
                if in_use(&self.current) || in_use(&self.pinned) {
                    self.pinned = None;
                    self.restart_now();
                }
                writeln!(reply, "ok")?;
            }
            Request::List => {
                let joined = self.current.as_deref().filter(|_| self.ready());
                reply.extend_from_slice(listing(&self.networks, joined).as_bytes());
            }
            Request::Scan => self.serve_scan(reply, report)?,
        }
        Ok(())
    }

    /// List what the radio can hear: one line per network, strongest first.
    fn serve_scan(&mut self, reply: &mut Vec<u8>, report: Report<'_>) -> io::Result<()> {
        if self.interface.is_none() {
            return Err(io::Error::other("no wireless interface"));
        }
        if !matches!(self.phase, Phase::Joining { .. } | Phase::Scanning { .. }) {
            let started = match self.scan() {
                Ok(()) => true,
                Err(e) if e.raw_os_error() == Some(16) => false,
                Err(e) => return Err(e),
            };
            // Other events, a rekey's EAPOL among them, are handled while
            // the scan runs rather than held for its ten seconds.
            let deadline = Instant::now() + SCAN_DEADLINE;
            let mut done = !started;
            while !done && Instant::now() < deadline {
                let Some(nl) = self.nl.as_mut() else { break };
                let left = deadline.saturating_duration_since(Instant::now());
                let (ready, _) = sys::poll2(Some(nl.socket.fd()), None, left)?;
                if ready {
                    nl.drain()?;
                }
                let events: Vec<(u8, Vec<u8>)> = nl.events.drain(..).collect();
                for (cmd, attributes) in events {
                    done |= matches!(cmd, genl::CMD_NEW_SCAN_RESULTS | genl::CMD_SCAN_ABORTED);
                    self.event(cmd, &attributes, report);
                }
            }
        }
        let mut seen: Vec<(i32, Vec<u8>, &'static str)> = Vec::new();
        for bss in self.results()? {
            let Some(ssid) = wpa::ssid(&bss.ies) else {
                continue;
            };
            if ssid.is_empty() || ssid.iter().all(|b| *b == 0) {
                continue;
            }
            let kind = match wpa::security(&bss.ies) {
                wpa::Security::Open => "open",
                wpa::Security::Wpa2Psk(_) => "wpa2",
                wpa::Security::Unsupported(_) => "unsupported",
            };
            match seen.iter_mut().find(|(_, s, _)| s == ssid) {
                Some(entry) if entry.0 < bss.signal_mbm => {
                    *entry = (bss.signal_mbm, ssid.to_vec(), kind)
                }
                Some(_) => {}
                None => seen.push((bss.signal_mbm, ssid.to_vec(), kind)),
            }
        }
        seen.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        for (signal, ssid, kind) in seen {
            writeln!(
                reply,
                "{:>4} dBm  {kind:<11}  {}",
                signal / 100,
                show_ssid(&ssid)
            )?;
        }
        Ok(())
    }
}

/// A request on the owner's socket.
enum Request {
    Join(Network),
    /// One network by SSID, or, for the line `forget` alone, every one.
    Forget(Option<Vec<u8>>),
    List,
    Scan,
}

fn parse_request(line: &str) -> io::Result<Request> {
    let malformed = |what: &str| io::Error::new(io::ErrorKind::InvalidInput, what.to_string());
    let (verb, rest) = match line.split_once(' ') {
        Some((verb, rest)) => (verb, Some(rest)),
        None => (line, None),
    };
    match (verb, rest) {
        ("join", Some(rest)) => decode_network(rest)
            .map(Request::Join)
            .ok_or_else(|| malformed("malformed join request")),
        ("forget", None) => Ok(Request::Forget(None)),
        ("forget", Some(hex)) => from_hex(hex)
            .filter(|ssid| (1..=32).contains(&ssid.len()))
            .map(|ssid| Request::Forget(Some(ssid)))
            .ok_or_else(|| malformed("malformed SSID")),
        ("list", None) => Ok(Request::List),
        ("scan", None) => Ok(Request::Scan),
        _ => Err(malformed("unknown request")),
    }
}

/// The saved list after joining `network`: it first, any earlier entry for
/// its SSID gone, the least recent dropped past `MAX_NETWORKS`.
fn joined(saved: &[Saved], network: Network) -> Vec<Saved> {
    let mut networks: Vec<Saved> = saved
        .iter()
        .filter(|s| s.network.ssid != network.ssid)
        .cloned()
        .collect();
    networks.insert(0, Saved::new(network));
    networks.truncate(MAX_NETWORKS);
    networks
}

/// The saved list without `ssid`, or empty for every network.
fn forgotten(saved: &[Saved], ssid: Option<&[u8]>) -> io::Result<Vec<Saved>> {
    let Some(ssid) = ssid else {
        return Ok(Vec::new());
    };
    let kept: Vec<Saved> = saved
        .iter()
        .filter(|s| s.network.ssid != ssid)
        .cloned()
        .collect();
    if kept.len() == saved.len() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no saved network has that name",
        ));
    }
    Ok(kept)
}

/// `list`'s reply: each saved network's security and name, never its key.
fn listing(saved: &[Saved], joined: Option<&[u8]>) -> String {
    let mut out = String::new();
    for saved in saved {
        let network = &saved.network;
        let kind = if network.pmk.is_some() {
            "wpa2"
        } else {
            "open"
        };
        let mark = if joined == Some(network.ssid.as_slice()) {
            "  (joined)"
        } else {
            ""
        };
        out.push_str(&format!("{kind}  {}{mark}\n", show_ssid(&network.ssid)));
    }
    out
}

/// Which saved network to join, by its index, and through which BSS: the
/// pinned network alone, or else the first in the saved order with a BSS in
/// range that fits it, passing over one resting after a failure while
/// another fits. Its strongest BSS, as `wpa::choose` picks it.
fn select<'a>(
    bsses: &'a [genl::Bss],
    saved: &[Saved],
    pinned: Option<&[u8]>,
    now: Instant,
) -> Result<(usize, &'a genl::Bss), String> {
    let mut resting = None;
    let mut refusals = Vec::new();
    for (index, candidate) in saved.iter().enumerate() {
        let network = &candidate.network;
        if pinned.is_some_and(|ssid| ssid != network.ssid.as_slice()) {
            continue;
        }
        match wpa::choose(bsses, &network.ssid, network.pmk.is_some()) {
            Ok(bss) if !candidate.resting(now) => return Ok((index, bss)),
            Ok(bss) => {
                resting.get_or_insert((index, bss));
            }
            Err(why) => refusals.push((show_ssid(&network.ssid), why)),
        }
    }
    if let Some(choice) = resting {
        return Ok(choice);
    }
    // A network in range that does not fit says why, as the only network
    // saved does; the rest are simply out of range.
    let heard = refusals.iter().find(|(_, why)| why != "network not found");
    match (refusals.as_slice(), heard) {
        ([(ssid, why)], _) | (_, Some((ssid, why))) => Err(format!("cannot join {ssid}: {why}")),
        _ => Err("no saved network is in range".into()),
    }
}

fn mac_text(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

// ── The owner's client: `td-netd wifi ...` ──────────────────────────────────

fn ask(request: &str, timeout: Duration) -> io::Result<String> {
    let mut stream = UnixStream::connect(CONTROL_PATH).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot reach the network service at {CONTROL_PATH}: {e}"),
        )
    })?;
    stream.set_read_timeout(Some(timeout))?;
    stream.write_all(format!("{request}\n").as_bytes())?;
    let mut reply = String::new();
    stream.take(1 << 20).read_to_string(&mut reply)?;
    if let Some(error) = reply.strip_prefix("error ") {
        return Err(io::Error::other(error.trim_end().to_string()));
    }
    Ok(reply)
}

/// A WPA2 passphrase (8 to 63 printable ASCII characters) or a 64-digit
/// hex PSK, to the PMK.
pub fn pmk_from(secret: &str, ssid: &[u8]) -> Result<[u8; 32], &'static str> {
    if secret.len() == 64 && secret.bytes().all(|b| b.is_ascii_hexdigit()) {
        return from_hex(secret)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or("malformed hex PSK");
    }
    if !(8..=63).contains(&secret.len()) || !secret.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Err("a WPA2 passphrase is 8 to 63 printable ASCII characters");
    }
    let mut pmk = [0u8; 32];
    crate::sha1::pbkdf2_hmac_sha1(secret.as_bytes(), ssid, 4096, &mut pmk)?;
    Ok(pmk)
}

fn read_passphrase(ssid: &[u8]) -> io::Result<String> {
    let terminal = sys::stdin_echo(false)?;
    if terminal {
        let _ = write!(io::stderr(), "Passphrase for {}: ", show_ssid(ssid));
        let _ = io::stderr().flush();
    }
    let mut line = String::new();
    let result = io::stdin().lock().read_line(&mut line);
    if terminal {
        sys::stdin_echo(true)?;
        let _ = writeln!(io::stderr());
    }
    result?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

pub fn client(args: &[String]) -> io::Result<std::process::ExitCode> {
    let usage = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: td-netd wifi scan | list | join [--open] [--] NAME | forget [--] NAME | forget --all",
        )
    };
    match args {
        [verb] if verb == "scan" => {
            print!("{}", ask("scan", Duration::from_secs(40))?);
            Ok(std::process::ExitCode::SUCCESS)
        }
        [verb] if verb == "list" => {
            let reply = ask("list", Duration::from_secs(5))?;
            if reply.is_empty() {
                println!("td-netd: no saved Wi-Fi networks");
            }
            print!("{reply}");
            Ok(std::process::ExitCode::SUCCESS)
        }
        [verb, all] if verb == "forget" && all == "--all" => {
            ask("forget", Duration::from_secs(5))?;
            println!("td-netd: every saved Wi-Fi network forgotten");
            Ok(std::process::ExitCode::SUCCESS)
        }
        [verb, rest @ ..] if verb == "forget" => {
            let name = match rest {
                [end, name] if end == "--" => name,
                [name] if !name.starts_with("--") => name,
                _ => return Err(usage()),
            };
            if name.is_empty() || name.len() > 32 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "an SSID is 1 to 32 bytes",
                ));
            }
            ask(
                &format!("forget {}", to_hex(name.as_bytes())),
                Duration::from_secs(5),
            )?;
            println!("td-netd: {} forgotten", show_ssid(name.as_bytes()));
            Ok(std::process::ExitCode::SUCCESS)
        }
        [verb, rest @ ..] if verb == "join" => {
            let (open, name) = match rest {
                [flag, name] if flag == "--open" && !name.starts_with("--") => (true, name),
                [flag, end, name] if flag == "--open" && end == "--" => (true, name),
                [end, name] if end == "--" => (false, name),
                [name] if !name.starts_with("--") => (false, name),
                _ => return Err(usage()),
            };
            let ssid = name.as_bytes().to_vec();
            if ssid.is_empty() || ssid.len() > 32 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "an SSID is 1 to 32 bytes",
                ));
            }
            let pmk = if open {
                None
            } else {
                let secret = read_passphrase(&ssid)?;
                Some(
                    pmk_from(&secret, &ssid)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
                )
            };
            let network = Network { ssid, pmk };
            let reply = ask(
                &format!("join {}", encode_network(&network)),
                Duration::from_secs(5),
            )?;
            match reply.trim_end().strip_prefix("ok") {
                Some(" wired") => {
                    println!(
                        "td-netd: saved; Ethernet holds the connection, so Wi-Fi joins once it is unplugged"
                    );
                    Ok(std::process::ExitCode::SUCCESS)
                }
                Some(rest) if rest.starts_with(' ') => watch_join(rest.trim_start()),
                _ => {
                    println!("td-netd: saved; no wireless interface is present yet");
                    Ok(std::process::ExitCode::SUCCESS)
                }
            }
        }
        _ => Err(usage()),
    }
}

/// What a status line says about a join on `interface`: success, failure,
/// or neither yet. Every failure goes through `Wifi::fail` (or watch's DHCP
/// retry), which ends the line with the retry; the SSID, which comes off
/// the air, is quoted before that, so it cannot forge the ending.
fn join_outcome(interface: &str, status: &str) -> Option<bool> {
    let line = status.strip_prefix(interface)?.strip_prefix(": ")?;
    if line.starts_with("connected, address") {
        return Some(true);
    }
    let (_, tail) = line.rsplit_once("; retrying in ")?;
    let seconds = tail.strip_suffix(" seconds")?;
    (!seconds.is_empty() && seconds.bytes().all(|b| b.is_ascii_digit())).then_some(false)
}

/// Follow the status file until the join on `interface` succeeds or fails,
/// or a minute.
fn watch_join(interface: &str) -> io::Result<std::process::ExitCode> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last = String::new();
    while Instant::now() < deadline {
        let status = fs::read_to_string(crate::STATUS_PATH).unwrap_or_default();
        let status = status.trim_end().to_string();
        if status != last {
            println!("{status}");
            match join_outcome(interface, &status) {
                Some(true) => return Ok(std::process::ExitCode::SUCCESS),
                Some(false) => return Ok(std::process::ExitCode::FAILURE),
                None => {}
            }
            last = status;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    println!("td-netd: still joining; `td-netd status` shows progress");
    Ok(std::process::ExitCode::FAILURE)
}

/// Zero a key an action carried when it is dropped unused.
fn forget(action: wpa::Action) {
    match action {
        wpa::Action::PairwiseKey { mut tk } => {
            tk.fill(0);
            std::hint::black_box(&mut tk);
        }
        wpa::Action::GroupKey { mut key, .. } => {
            key.fill(0);
            std::hint::black_box(&mut key);
        }
        wpa::Action::Send { .. } | wpa::Action::Authorize => {}
    }
}

const WRONG_PASSPHRASE: &str =
    "the access point ended the key handshake; the passphrase is likely wrong";

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn network_round_trips_and_bounds() {
        let network = Network {
            ssid: b"home net".to_vec(),
            pmk: Some([7; 32]),
        };
        let line = encode_network(&network);
        assert_eq!(line, format!("686f6d65206e6574 {}", "07".repeat(32)));
        assert!(decode_network(&line) == Some(network));
        let open = Network {
            ssid: vec![0, 0xff],
            pmk: None,
        };
        assert!(decode_network(&encode_network(&open)) == Some(open));
        for bad in [
            "",
            "61",
            "61 open extra",
            " open",
            "6 open",
            "zz open",
            "61 0707",
            &format!("{} open", "61".repeat(33)),
        ] {
            assert!(decode_network(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn the_saved_list_keeps_order_and_drops_what_does_not_decode() {
        let line = |name: &[u8], pmk| {
            encode_network(&Network {
                ssid: name.to_vec(),
                pmk,
            })
        };
        let text = format!(
            "{}\n{}\nnot a network\n{}\n",
            line(b"home", Some([1; 32])),
            line(b"cafe", None),
            line(b"home", Some([2; 32])),
        );
        let networks = decode_networks(&text);
        let names: Vec<&[u8]> = networks.iter().map(|n| n.ssid.as_slice()).collect();
        assert_eq!(names, vec![&b"home"[..], b"cafe"]);
        assert_eq!(networks[0].pmk, Some([1; 32]));
        // The single-network file earlier td-netd wrote is a one-line list.
        assert_eq!(decode_networks(&line(b"home", None)).len(), 1);
        let many: String = (0..MAX_NETWORKS + 3)
            .map(|n| format!("{}\n", line(format!("net{n}").as_bytes(), None)))
            .collect();
        assert_eq!(decode_networks(&many).len(), MAX_NETWORKS);
        let longest = line(&[0xff; 32], Some([0xff; 32])).len() + 1;
        assert!((MAX_NETWORKS * longest) as u64 <= MAX_CONFIG);
    }

    fn bss(last: u8, name: &[u8], protected: bool, signal: i32) -> genl::Bss {
        let mut ies = vec![0, name.len() as u8];
        ies.extend_from_slice(name);
        if protected {
            ies.extend(from_hex("30140100000fac040100000fac040100000fac020c00").unwrap());
        }
        genl::Bss {
            bssid: [2, 0, 0, 0, 0, last],
            frequency: 2412,
            signal_mbm: signal,
            ies,
        }
    }

    fn saved(name: &[u8], protected: bool) -> Saved {
        Saved::new(Network {
            ssid: name.to_vec(),
            pmk: protected.then_some([9; 32]),
        })
    }

    #[test]
    fn the_most_recent_saved_network_in_range_is_chosen() {
        let now = Instant::now();
        let air = [
            bss(1, b"cafe", false, -3000),
            bss(2, b"home", true, -7000),
            bss(3, b"home", true, -5000),
        ];
        let list = [
            saved(b"office", true),
            saved(b"home", true),
            saved(b"cafe", false),
        ];
        let (index, chosen) = select(&air, &list, None, now).unwrap();
        assert_eq!((index, chosen.bssid[5]), (1, 3));
        // A pin names the one network an attempt may join.
        let (index, _) = select(&air, &list, Some(b"cafe"), now).unwrap();
        assert_eq!(index, 2);
        assert_eq!(
            select(&air, &list, Some(b"office"), now).unwrap_err(),
            "cannot join \"office\": network not found"
        );
        // A network resting after a failure gives way to another in range,
        // and is still taken when nothing else fits.
        let mut list = list;
        list[1].rest_until = Some(now + Duration::from_secs(30));
        assert_eq!(select(&air, &list, None, now).unwrap().0, 2);
        assert_eq!(select(&air[1..], &list, None, now).unwrap().0, 1);
        assert_eq!(
            select(&air[..1], &list[..2], None, now).unwrap_err(),
            "no saved network is in range"
        );
        assert!(select(&air, &[saved(b"cafe", true)], None, now)
            .unwrap_err()
            .contains("--open"));
        // Among several saved, the one heard but unfit still says why.
        let heard = select(
            &air[..1],
            &[saved(b"office", true), saved(b"cafe", true)],
            None,
            now,
        );
        assert!(heard.unwrap_err().starts_with("cannot join \"cafe\": "));
    }

    #[test]
    fn a_failure_rests_the_network_that_failed_and_ends_the_pin() {
        let mut lines = Vec::new();
        let mut wifi = Wifi::new(None, &mut |_| {});
        wifi.networks = vec![saved(b"a", true), saved(b"b", true)];
        wifi.pinned = Some(b"b".to_vec());
        wifi.current = Some(b"b".to_vec());
        wifi.fail(&mut |line| lines.push(line.to_string()), "wifi: x".into());
        assert!(wifi.pinned.is_none() && wifi.current.is_none());
        assert!(wifi.networks[1].resting(Instant::now()));
        assert!(!wifi.networks[0].resting(Instant::now()));
        assert_eq!(lines, vec!["wifi: x; retrying in 2 seconds"]);
        wifi.current = Some(b"b".to_vec());
        wifi.authorized([0; 6], None, &mut |_| {});
        assert!(!wifi.networks[1].resting(Instant::now()));
        assert_eq!(wifi.networks[1].failures, 0);
    }

    #[test]
    fn a_failing_network_gives_way_at_the_next_scan_however_long_it_runs() {
        let air = [bss(1, b"home", true, -4000), bss(2, b"cafe", true, -6000)];
        let mut wifi = Wifi::new(None, &mut |_| {});
        wifi.networks = vec![saved(b"home", true), saved(b"cafe", true)];
        for round in 0..6 {
            // The next selection is the retry plus a scan, at most the
            // scan's deadline.
            wifi.current = Some(b"home".to_vec());
            wifi.fail(&mut |_| {}, "wifi: x".into());
            let next = Instant::now() + backoff(wifi.failures) + SCAN_DEADLINE;
            let (index, _) = select(&air, &wifi.networks, None, next).unwrap();
            assert_eq!(index, 1, "round {round}");
            // With nothing else in range it is still taken.
            assert_eq!(select(&air[..1], &wifi.networks, None, next).unwrap().0, 0);
        }
    }

    #[test]
    fn requests_parse_strictly() {
        let pmk = "ab".repeat(32);
        assert!(matches!(
            parse_request(&format!("join 686f6d65 {pmk}")).unwrap(),
            Request::Join(n) if n.ssid == b"home" && n.pmk == Some([0xab; 32])
        ));
        assert!(matches!(
            parse_request("forget").unwrap(),
            Request::Forget(None)
        ));
        assert!(matches!(
            parse_request("forget 686f6d65").unwrap(),
            Request::Forget(Some(ssid)) if ssid == b"home"
        ));
        assert!(matches!(parse_request("list").unwrap(), Request::List));
        assert!(matches!(parse_request("scan").unwrap(), Request::Scan));
        for bad in [
            "forget ",
            "forget zz",
            &format!("forget {}", "61".repeat(33)),
            "join",
            "join 686f6d65",
            "list ",
            "scan now",
            "",
            "Forget",
            "forget +a",
            &format!("join +a {pmk}"),
        ] {
            assert!(parse_request(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn joining_forgetting_and_listing() {
        let network = |name: &[u8], pmk| Network {
            ssid: name.to_vec(),
            pmk,
        };
        let mut list: Vec<Saved> = Vec::new();
        for n in 0..MAX_NETWORKS {
            list = joined(&list, network(format!("net{n}").as_bytes(), None));
        }
        list = joined(&list, network(b"home", Some([0x5a; 32])));
        assert_eq!(list.len(), MAX_NETWORKS);
        assert_eq!(list[0].network.ssid, b"home");
        assert!(!list.iter().any(|s| s.network.ssid == b"net0"));
        // Joining a saved network again moves it first, with its new key.
        list = joined(&list, network(b"net5", Some([1; 32])));
        assert_eq!(list[0].network.ssid, b"net5");
        assert_eq!(list.iter().filter(|s| s.network.ssid == b"net5").count(), 1);

        let text = listing(&list[..2], Some(b"home"));
        assert_eq!(text, "wpa2  \"net5\"\nwpa2  \"home\"  (joined)\n");
        assert!(!text.contains("5a5a") && !text.contains("0101"));

        let kept = forgotten(&list, Some(b"home")).unwrap();
        assert_eq!(kept.len(), list.len() - 1);
        // No Debug for a list of keys, so `err()` rather than `unwrap_err()`.
        assert_eq!(
            forgotten(&kept, Some(b"home")).err().map(|e| e.kind()),
            Some(io::ErrorKind::NotFound)
        );
        assert!(forgotten(&kept, None).unwrap().is_empty());
    }

    #[test]
    fn scans_probe_recent_names_within_the_limit() {
        let mut wifi = Wifi::new(None, &mut |_| {});
        wifi.networks = vec![
            saved(b"a", true),
            saved(b"b", true),
            saved(b"c", true),
            saved(b"d", true),
        ];
        let names = |wifi: &Wifi| wifi.probes().len();
        assert_eq!(names(&wifi), PROBE_LIMIT - 1);
        wifi.probe_limit = 2;
        assert_eq!(wifi.probes(), vec![b"a".to_vec()]);
        wifi.pinned = Some(b"c".to_vec());
        assert_eq!(wifi.probes(), vec![b"c".to_vec()]);
        wifi.probe_limit = 1;
        assert!(wifi.probes().is_empty());
        wifi.pinned = None;
        assert!(wifi.probes().is_empty());
    }

    #[test]
    fn ssids_are_escaped_for_display() {
        assert_eq!(show_ssid(b"home"), "\"home\"");
        assert_eq!(show_ssid(b"a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(show_ssid(&[0x1b, b'[', 0xff]), "\"\\x1b[\\xff\"");
    }

    #[test]
    fn passphrases_and_hex_psks() {
        let pmk = pmk_from("password", b"IEEE").unwrap();
        assert_eq!(
            to_hex(&pmk),
            "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e"
        );
        let hex = "ab".repeat(32);
        assert_eq!(pmk_from(&hex, b"x").unwrap(), [0xab; 32]);
        for bad in [
            "short",
            &"x".repeat(64),
            &"a".repeat(65),
            "tab\there!",
            "é-accented",
        ] {
            assert!(pmk_from(bad, b"x").is_err(), "{bad:?}");
        }
    }

    #[test]
    fn join_outcomes_are_read_from_the_interfaces_own_lines() {
        let ok = "wlan0: connected, address 192.0.2.7/24, gateway 192.0.2.1";
        assert_eq!(join_outcome("wlan0", ok), Some(true));
        assert_eq!(join_outcome("eth0", ok), None);
        for failure in [
            "wlan0: joining \"home\" failed: association timed out; retrying in 2 seconds",
            "wlan0: DHCP failed: timed out; retrying in 15 seconds",
            "wlan0: cannot join \"x\": no such network; retrying in 60 seconds",
        ] {
            assert_eq!(join_outcome("wlan0", failure), Some(false), "{failure}");
        }
        for pending in [
            "wlan0: joining \"failed; retrying in 9 seconds\"",
            "wlan0: scanning for \"refused\"",
            "wlan0: joined \"home\"; requesting DHCP lease",
            "eth0: DHCP failed: timed out; retrying in 15 seconds",
            "wlan0: joining \"x\" failed; retrying in  seconds",
        ] {
            assert_eq!(join_outcome("wlan0", pending), None, "{pending}");
        }
    }

    #[test]
    fn backoff_grows_and_caps() {
        let waits: Vec<u64> = (0..8).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(waits, vec![2, 2, 5, 10, 30, 60, 60, 60]);
    }

    #[test]
    fn a_new_station_without_owner_binds_nothing() {
        let mut lines = Vec::new();
        let wifi = Wifi::new(None, &mut |line| lines.push(line.to_string()));
        assert!(wifi.control.is_none());
        assert!(!wifi.ready());
        assert!(lines.is_empty());
    }
}
