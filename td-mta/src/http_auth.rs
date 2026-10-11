//! HTTPS request authentication and bounded attempt admission.
use crate::{
    config::endpoint::Origin,
    ids::DeviceId,
    store_fs::{DeviceError, DeviceStore},
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
/// Call only after the actual TLS handshake and configured-origin checks.
/// This observation is not a mutation guard or permanent connection authority.
pub fn basic(
    store: &DeviceStore<'_>,
    username: &str,
    header: Option<&str>,
) -> Result<Option<DeviceId>, DeviceError> {
    let Some((scheme, token)) = header.and_then(|header| header.split_once(' ')) else {
        return Ok(None);
    };
    if !scheme.eq_ignore_ascii_case("Basic") {
        return Ok(None);
    }
    let token = token.trim_start_matches(' ').as_bytes();
    let mut credentials = Credentials([0; DECODED]);
    let Some(length) = decode(token, &mut credentials.0) else {
        return Ok(None);
    };
    let Some(decoded) = credentials.0.get(..length) else {
        return Ok(None);
    };
    let Some(colon) = decoded.iter().position(|byte| *byte == b':') else {
        return Ok(None);
    };
    if decoded.get(..colon) != Some(username.as_bytes()) {
        return Ok(None);
    }
    store.authenticate(decoded.get(colon + 1..).ok_or(DeviceError::Invalid)?)
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
    pub fn admit(&mut self, address: IpAddr, monotonic_ms: u64) -> bool {
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
    fn rate_windows_bound_global_and_peer_work() {
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
    }
}
