//! HTTPS request authentication and bounded attempt admission.
use crate::{
    config::endpoint::Origin,
    ids::{AccountId, DeviceId},
    ports::{Deadline, Error},
};
use std::net::IpAddr;

const DECODED: usize = 254 + 1 + 97;
struct Credentials([u8; DECODED]);
impl Drop for Credentials {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&mut self.0);
    }
}
/// Presented bytes only, never proof of an active device or account access.
/// Keep on the bounded request worker and discard when that request finishes.
/// Deliberately neither Debug nor Clone; Drop clears the decoded buffer.
pub struct BasicCredentials {
    decoded: Credentials,
    username: std::ops::Range<usize>,
    password: std::ops::Range<usize>,
}
impl BasicCredentials {
    pub fn username(&self) -> &str {
        self.decoded
            .0
            .get(self.username.clone())
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .unwrap_or("")
    }
    /// Borrow only for fresh storage authentication or publication fencing.
    pub fn password(&self) -> &[u8] {
        self.decoded.0.get(self.password.clone()).unwrap_or(&[])
    }
}

/// Parse only after the actual TLS handshake, configured-origin checks and
/// attempt admission. The caller must authenticate these bytes against its
/// storage backend; successful parsing grants no authority.
pub fn parse_basic(header: Option<&str>) -> Option<BasicCredentials> {
    let (scheme, token) = header?.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Basic") {
        return None;
    }
    let token = token.trim_start_matches(' ').as_bytes();
    let mut credentials = Credentials([0; DECODED]);
    let length = decode(token, &mut credentials.0)?;
    let decoded = credentials.0.get(..length)?;
    let colon = decoded.iter().position(|byte| *byte == b':')?;
    let username = decoded.get(..colon)?;
    if username.is_empty()
        || username.len() > 254
        || !username.iter().all(|byte| (33..=126).contains(byte))
        || length.checked_sub(colon + 1)? > 97
    {
        return None;
    }
    Some(BasicCredentials {
        decoded: credentials,
        username: 0..colon,
        password: colon + 1..length,
    })
}

/// A fresh authentication result, scoped to both account and device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Principal {
    pub account: AccountId,
    pub device: DeviceId,
}

/// Call only after the actual TLS handshake and configured-origin checks.
/// This observation is not a mutation guard or permanent connection authority.
pub trait Authenticator: Sync {
    /// Called periodically on the fixed authentication worker, outside slots.
    /// Retire local idle resources without issuing authorization queries.
    /// Busy defers cleanup to the next maintenance turn; other errors stop serving.
    fn maintain(&self) -> Result<(), Error> {
        Ok(())
    }
    /// Release retained resources on their owning thread before it exits.
    fn close(&self) -> Result<(), Error> {
        Ok(())
    }
    fn authenticate(
        &self,
        username: &str,
        password: &[u8],
        deadline: Deadline,
    ) -> Result<Option<Principal>, Error>;
}
fn sextet(byte: u8) -> Option<u32> {
    Some(u32::from(match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    }))
}
fn decode(token: &[u8], output: &mut [u8]) -> Option<usize> {
    if token.is_empty() || token.len() > DECODED.div_ceil(3) * 4 || !token.len().is_multiple_of(4) {
        return None;
    }
    let (groups, _) = token.as_chunks::<4>();
    let mut written = 0;
    for (index, &[a, b, c, d]) in groups.iter().enumerate() {
        if c == b'=' && d != b'=' {
            return None;
        }
        let padding = if c == b'=' && d == b'=' {
            2
        } else if d == b'=' {
            1
        } else {
            0
        };
        if padding != 0 && index + 1 != groups.len() {
            return None;
        }
        let bits = (sextet(a)? << 18)
            | (sextet(b)? << 12)
            | (if c == b'=' { 0 } else { sextet(c)? } << 6)
            | if d == b'=' { 0 } else { sextet(d)? };
        if (padding == 2 && bits & 0xffff != 0) || (padding == 1 && bits & 0xff != 0) {
            return None;
        }
        let bytes = [(bits >> 16) as u8, (bits >> 8) as u8, bits as u8];
        let count = 3 - padding;
        output
            .get_mut(written..written + count)?
            .copy_from_slice(bytes.get(..count)?);
        written += count;
    }
    Some(written)
}
/// Only the configured DNS origin can route requests. Host never builds URLs.
pub fn same_authority(origin: Origin<'_>, host: &str) -> bool {
    let (name, port) = match host.split_once(':') {
        Some((name, port)) => match port.parse::<u16>() {
            Ok(port) => (name, port),
            Err(_) => return false,
        },
        None => (host, 443),
    };
    name.eq_ignore_ascii_case(origin.host()) && port == origin.port()
}

const PEERS: usize = 64;
const GLOBAL_ATTEMPTS: u16 = 64;
const PEER_ATTEMPTS: u16 = 8;
#[derive(Clone, Copy)]
struct Peer {
    address: IpAddr,
    attempts: u16,
}
/// One charged attempt; only a successful authentication returns its peer charge.
/// Not Clone or Copy: completion cannot refund the same reservation twice.
pub(crate) struct Attempt {
    address: IpAddr,
    second: u64,
}
/// Fixed one-second windows. Never evict an unexpired peer to admit a new one.
pub struct Attempts {
    second: u64,
    global: u16,
    peers: [Option<Peer>; PEERS],
}
impl Default for Attempts {
    fn default() -> Self {
        Self {
            second: 0,
            global: 0,
            peers: [None; PEERS],
        }
    }
}
impl Attempts {
    pub(crate) fn reserve(&mut self, address: IpAddr, monotonic_ms: u64) -> Option<Attempt> {
        self.admit(address, monotonic_ms).then_some(Attempt {
            address,
            second: monotonic_ms / 1000,
        })
    }
    pub(crate) fn authenticated(&mut self, attempt: Attempt) {
        if attempt.second != self.second {
            return;
        }
        if let Some(peer) = self
            .peers
            .iter_mut()
            .flatten()
            .find(|peer| peer.address == attempt.address)
        {
            peer.attempts = peer.attempts.saturating_sub(1);
        }
    }
    pub fn admit(&mut self, address: IpAddr, monotonic_ms: u64) -> bool {
        let admitted = self.try_admit(address, monotonic_ms);
        if !admitted {
            crate::performance::record(
                crate::performance::Phase::HttpAuthRefused,
                std::time::Duration::ZERO,
                crate::performance::Outcome::Busy,
            );
        }
        admitted
    }
    fn try_admit(&mut self, address: IpAddr, monotonic_ms: u64) -> bool {
        let second = monotonic_ms / 1000;
        if second < self.second {
            return false;
        }
        if second > self.second {
            self.second = second;
            self.global = 0;
            self.peers.fill(None);
        }
        if self.global >= GLOBAL_ATTEMPTS {
            return false;
        }
        let existing = self
            .peers
            .iter()
            .position(|peer| peer.is_some_and(|peer| peer.address == address));
        let Some(index) = existing.or_else(|| self.peers.iter().position(Option::is_none)) else {
            return false;
        };
        let Some(slot) = self.peers.get_mut(index) else {
            return false;
        };
        let peer = slot.get_or_insert(Peer {
            address,
            attempts: 0,
        });
        if peer.attempts >= PEER_ATTEMPTS {
            return false;
        }
        peer.attempts += 1;
        self.global += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parsed_credentials_are_owned_bounded_and_not_authentication() {
        let header = String::from("bAsIc   dTpw");
        let credentials = parse_basic(Some(&header));
        drop(header);
        assert_eq!(
            credentials.as_ref().map(BasicCredentials::password),
            Some(&b"p"[..])
        );
        // Empty and malformed app passwords still require backend checking;
        // parsing must not turn their syntax into authentication evidence.
        assert_eq!(
            parse_basic(Some("Basic dTo="))
                .as_ref()
                .map(BasicCredentials::password),
            Some(&b""[..])
        );
        for header in [
            None,
            Some("Bearer dTpw"),
            Some("Basic dQ=="),
            Some("Basic dTp="),
            Some("Basic dTpw\n"),
            Some("Basic dTpw "),
        ] {
            assert!(parse_basic(header).is_none());
        }
        assert_eq!(
            parse_basic(Some("Basic dTpw")).map(|value| value.username() == "u"),
            Some(true)
        );
        assert!(parse_basic(Some("Basic OnA=")).is_none());
        assert!(parse_basic(Some("Basic IHU6cA==")).is_none());
        let oversized = format!("Basic {}", "dTpw".repeat(DECODED));
        assert!(parse_basic(Some(&oversized)).is_none());
    }
    #[test]
    fn canonical_basic_base64_only() {
        let mut out = [0; DECODED];
        for (wire, plain) in [("dTpw", "u:p"), ("dTo=", "u:"), ("dQ==", "u")] {
            let n = decode(wire.as_bytes(), &mut out);
            assert_eq!(n.and_then(|n| out.get(..n)), Some(plain.as_bytes()));
        }
        for wire in [
            "", "dQ", "dQ=", "dQ===", "dQ==AAAA", "dR==", "dTp=", "dQ=A", "dQ==\n", "____", "====",
        ] {
            assert_eq!(decode(wire.as_bytes(), &mut out), None, "{wire}");
        }
    }
    #[test]
    #[allow(clippy::unwrap_used)]
    fn rate_windows_bound_global_and_peer_work() {
        let _serial = crate::generations::TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before =
            crate::performance::snapshot(crate::performance::Phase::HttpAuthRefused).unwrap();
        let mut attempts = Attempts::default();
        let ip = IpAddr::from([127, 0, 0, 1]);
        for _ in 0..PEER_ATTEMPTS {
            assert!(attempts.admit(ip, 1));
        }
        assert!(!attempts.admit(ip, 999));
        for octet in 2..=8 {
            for _ in 0..PEER_ATTEMPTS {
                assert!(attempts.admit(IpAddr::from([127, 0, 0, octet]), 999));
            }
        }
        assert!(!attempts.admit(IpAddr::from([127, 0, 0, 9]), 999));
        assert!(attempts.admit(ip, 1000));
        assert!(!attempts.admit(ip, 999));
        let after =
            crate::performance::snapshot(crate::performance::Phase::HttpAuthRefused).unwrap();
        assert!(after.calls >= before.calls + 3);
        assert_eq!(after.busy - before.busy, after.calls - before.calls);
        assert_eq!(after.total_us, 0);
    }
    #[test]
    #[allow(clippy::unwrap_used)]
    fn successful_authentication_releases_only_its_peer_charge() {
        let ip = IpAddr::from([127, 0, 0, 1]);
        let mut attempts = Attempts::default();
        for _ in 0..GLOBAL_ATTEMPTS {
            let admitted = attempts.reserve(ip, 1).unwrap();
            attempts.authenticated(admitted);
        }
        assert!(attempts.reserve(ip, 1).is_none());
        let mut attempts = Attempts::default();
        for _ in 0..PEER_ATTEMPTS - 1 {
            assert!(attempts.reserve(ip, 1).is_some());
        }
        let admitted = attempts.reserve(ip, 1).unwrap();
        assert!(attempts.reserve(ip, 1).is_none());
        attempts.authenticated(admitted);
        assert!(attempts.reserve(ip, 1).is_some());
        assert!(attempts.reserve(ip, 1).is_none());
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn late_success_cannot_refund_a_new_window() {
        let ip = IpAddr::from([127, 0, 0, 1]);
        let mut attempts = Attempts::default();
        let old = attempts.reserve(ip, 999).unwrap();
        for _ in 0..PEER_ATTEMPTS {
            assert!(attempts.reserve(ip, 1000).is_some());
        }
        attempts.authenticated(old);
        assert!(attempts.reserve(ip, 1000).is_none());
        assert!(attempts.reserve(ip, 999).is_none());
    }
}
