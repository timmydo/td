//! The listening process: root, holds no key material past its start-up check,
//! parses no SSH, and caps how many unauthenticated connections exist at once.

use super::account::SingleUser;
use super::config::ServerConfig;
use super::keys;
use super::log;
use crate::sys;
use std::collections::HashMap;
use std::io::Read;
use std::net::{IpAddr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const MAX_UNAUTHENTICATED: u32 = 64;
pub const MAX_UNAUTHENTICATED_PER_SOURCE: u32 = 8;
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_millis(100);

const V6_HOST_BITS: u32 = 64;

/// What the per-source cap counts by: one IPv4 address, however it arrived, or
/// one IPv6 /64, which is what a single site is routinely handed.
fn source_key(address: IpAddr) -> IpAddr {
    match address.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(
            (u128::from(v6) >> V6_HOST_BITS) << V6_HOST_BITS,
        )),
        v4 => v4,
    }
}

/// Admits a new connection against both caps, or refuses it.
#[derive(Debug, Default)]
pub struct Admission {
    total: u32,
    per_source: HashMap<IpAddr, u32>,
}

impl Admission {
    pub fn try_admit(&mut self, address: IpAddr) -> bool {
        let source = source_key(address);
        let at_source = self.per_source.get(&source).copied().unwrap_or(0);
        if self.total >= MAX_UNAUTHENTICATED || at_source >= MAX_UNAUTHENTICATED_PER_SOURCE {
            return false;
        }
        self.total += 1;
        self.per_source.insert(source, at_source + 1);
        true
    }

    pub fn release(&mut self, address: IpAddr) {
        let source = source_key(address);
        self.total = self.total.saturating_sub(1);
        let remaining = self
            .per_source
            .get(&source)
            .copied()
            .unwrap_or(0)
            .saturating_sub(1);
        if remaining == 0 {
            self.per_source.remove(&source);
        } else {
            self.per_source.insert(source, remaining);
        }
    }
}

pub fn main(config_path: &str, single_user: Option<&SingleUser>) -> i32 {
    match run(config_path, single_user) {
        Ok(()) => 0,
        Err(error) => {
            log::line(format_args!("td-sshd: {error}"));
            1
        }
    }
}

/// `-t`, and the same refusal at start: boot health sees a bad key or policy as
/// the unit failing to become ready.
pub fn validate(
    config_path: &str,
    single_user: Option<&SingleUser>,
) -> Result<ServerConfig, String> {
    let config = read_config(config_path, single_user.is_some())?;
    keys::load_host_key(&config.host_key, single_user)?;
    Ok(config)
}

pub fn read_config(config_path: &str, allow_port_zero: bool) -> Result<ServerConfig, String> {
    let text =
        std::fs::read_to_string(config_path).map_err(|error| format!("{config_path}: {error}"))?;
    ServerConfig::parse(&text, allow_port_zero).map_err(|error| format!("{config_path}: {error}"))
}

fn run(config_path: &str, single_user: Option<&SingleUser>) -> Result<(), String> {
    // Every stage and account process inherits this; td-svc promises no umask.
    sys::raise_umask_floor().map_err(|error| format!("umask: {error}"))?;
    let config = validate(config_path, single_user)?;
    let listener = TcpListener::bind(SocketAddr::new(config.listen, config.port))
        .map_err(|error| format!("bind {}:{}: {error}", config.listen, config.port))?;
    let bound = listener
        .local_addr()
        .map_err(|error| format!("local address: {error}"))?;
    if config.port == 0 {
        log::line(format_args!("td-sshd: listening on port {}", bound.port()));
    } else {
        log::line(format_args!(
            "Server listening on {} port {}.",
            bound.ip(),
            bound.port()
        ));
    }
    let admission = Arc::new(Mutex::new(Admission::default()));
    loop {
        let (socket, peer) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(error) => {
                // Running out of descriptors must not spin or end the service.
                log::line(format_args!("td-sshd: accept: {error}"));
                std::thread::sleep(ACCEPT_ERROR_PAUSE);
                continue;
            }
        };
        let admitted = admission
            .lock()
            .map(|mut state| state.try_admit(peer.ip()))
            .unwrap_or(false);
        if !admitted {
            log::line(format_args!(
                "td-sshd: too many unauthenticated connections; dropping {}",
                peer.ip().to_canonical()
            ));
            continue;
        }
        if let Err(error) = spawn_connection(socket, peer, config_path, single_user, &admission) {
            log::line(format_args!("td-sshd: spawning a connection: {error}"));
            if let Ok(mut state) = admission.lock() {
                state.release(peer.ip());
            }
        }
    }
}

fn spawn_connection(
    socket: TcpStream,
    peer: SocketAddr,
    config_path: &str,
    single_user: Option<&SingleUser>,
    admission: &Arc<Mutex<Admission>>,
) -> std::io::Result<()> {
    let _ = socket.set_nodelay(true);
    let (child_sender, child_receiver) = std::sync::mpsc::channel::<Child>();
    let admission = admission.clone();
    let address = peer.ip();
    // The watcher exists first: failing to start one costs this connection, here
    // and with no process abandoned, and the caller releases its slot.
    std::thread::Builder::new().spawn(move || {
        let Ok(mut child) = child_receiver.recv() else {
            return;
        };
        // A byte means authenticated, EOF means the monitor ended. Both stop this
        // connection counting. The status pipe is closed by the monitor's exec.
        if let Some(mut status) = child.stdout.take() {
            let mut byte = [0u8; 1];
            let _ = status.read(&mut byte);
        }
        if let Ok(mut state) = admission.lock() {
            state.release(address);
        }
        let _ = child.wait();
    })?;
    let mut command = Command::new("/proc/self/exe");
    command.arg("--connection").arg("-f").arg(config_path);
    if let Some(single) = single_user {
        command
            .arg("--single-user")
            .arg(&single.name)
            .env("HOME", &single.home)
            .env("SHELL", &single.shell);
    }
    let child = command
        .stdin(Stdio::from(OwnedFd::from(socket)))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    // The watcher outlives this send unless the process is ending anyway.
    let _ = child_sender.send(child);
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn admission_caps_per_source_and_total_and_release_reopens() {
        let mut admission = Admission::default();
        let source = IpAddr::from([10, 0, 0, 1]);
        for _ in 0..MAX_UNAUTHENTICATED_PER_SOURCE {
            assert!(admission.try_admit(source));
        }
        assert!(!admission.try_admit(source));
        admission.release(source);
        assert!(admission.try_admit(source));

        let mut admission = Admission::default();
        for index in 0..MAX_UNAUTHENTICATED {
            let [a, b, c, d] = index.to_be_bytes();
            assert!(admission.try_admit(IpAddr::from([a, b, c, d])));
        }
        assert!(!admission.try_admit(IpAddr::from([9, 9, 9, 9])));
    }

    #[test]
    fn a_mapped_address_counts_as_its_ipv4_source() {
        let mut admission = Admission::default();
        let v4 = IpAddr::from([10, 0, 0, 1]);
        let mapped: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
        for index in 0..MAX_UNAUTHENTICATED_PER_SOURCE {
            assert!(admission.try_admit(if index % 2 == 0 { v4 } else { mapped }));
        }
        assert!(!admission.try_admit(v4));
        assert!(!admission.try_admit(mapped));
        admission.release(mapped);
        assert!(admission.try_admit(v4));
        // A different IPv4 source is a different source.
        assert!(admission.try_admit(IpAddr::from([10, 0, 0, 2])));
    }

    #[test]
    fn ipv6_counts_by_its_64() {
        let mut admission = Admission::default();
        for host in 1..=MAX_UNAUTHENTICATED_PER_SOURCE {
            let address: IpAddr = format!("2001:db8:0:1::{host:x}").parse().unwrap();
            assert!(admission.try_admit(address), "{address}");
        }
        let same: IpAddr = "2001:db8:0:1:ffff::1".parse().unwrap();
        assert!(!admission.try_admit(same));
        let next: IpAddr = "2001:db8:0:2::1".parse().unwrap();
        assert!(admission.try_admit(next));
        admission.release(same);
        assert!(admission.try_admit(same));
    }

    #[test]
    fn releasing_more_than_was_admitted_does_not_underflow() {
        let mut admission = Admission::default();
        admission.release(IpAddr::from([1, 1, 1, 1]));
        assert!(admission.try_admit(IpAddr::from([1, 1, 1, 1])));
    }
}
