//! HTTPS request authentication and bounded attempt admission.
use crate::{
    config::endpoint::Origin,
    ids::{AccountId, DeviceId},
    ports::{Access, Clock, Deadline, Error, Principal},
    store_fs::{DeviceError, DeviceStore},
};
use std::{
    net::IpAddr,
    sync::{Mutex, MutexGuard, TryLockError},
};

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
    password: std::ops::Range<usize>,
}
impl BasicCredentials {
    /// Borrow only for fresh storage authentication or publication fencing.
    pub fn password(&self) -> &[u8] {
        self.decoded.0.get(self.password.clone()).unwrap_or(&[])
    }
}

/// Parse only after the actual TLS handshake, configured-origin checks and
/// attempt admission. The caller must authenticate these bytes against its
/// storage backend; successful parsing grants no authority.
pub fn parse_basic(username: &str, header: Option<&str>) -> Option<BasicCredentials> {
    let (scheme, token) = header?.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Basic") {
        return None;
    }
    let token = token.trim_start_matches(' ').as_bytes();
    let mut credentials = Credentials([0; DECODED]);
    let length = decode(token, &mut credentials.0)?;
    let decoded = credentials.0.get(..length)?;
    let colon = decoded.iter().position(|byte| *byte == b':')?;
    if decoded.get(..colon) != Some(username.as_bytes()) {
        return None;
    }
    Some(BasicCredentials {
        decoded: credentials,
        password: colon + 1..length,
    })
}

/// Call only after the actual TLS handshake and configured-origin checks.
/// This observation is not a mutation guard or permanent connection authority.
pub fn basic(
    store: &DeviceStore<'_>,
    username: &str,
    header: Option<&str>,
) -> Result<Option<DeviceId>, DeviceError> {
    match parse_basic(username, header) {
        Some(credentials) => store.authenticate(credentials.password()),
        None => Ok(None),
    }
}

/// Bounded credential lookup on the fixed authentication worker, never the
/// network reactor. Busy retries retain the original request deadline and
/// attempt admission. Success is an observation, not a later mutation guard.
pub trait Authenticator: Sync {
    fn authenticate(&self, password: &[u8], deadline: Deadline) -> Result<Option<DeviceId>, Error>;
}
impl Authenticator for Mutex<DeviceStore<'_>> {
    fn authenticate(
        &self,
        password: &[u8],
        _deadline: Deadline,
    ) -> Result<Option<DeviceId>, Error> {
        let store = match self.try_lock() {
            Ok(store) => store,
            Err(TryLockError::WouldBlock) => return Err(Error::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(Error::WriterStopped),
        };
        store.authenticate(password).map_err(|_| Error::Invalid)
    }
}

/// Binds upload commits and blob-read admission to the live device registry. Callers may
/// supply only the device ID obtained by successful HTTPS authentication.
/// The fixed configuration generation is retained for the service lifetime;
/// a future reload must replace policy under the same publication exclusion.
pub struct DevicePolicy<'a, 'r> {
    devices: &'a Mutex<DeviceStore<'r>>,
    clock: &'a dyn Clock,
    generation: u64,
}
impl<'a, 'r> DevicePolicy<'a, 'r> {
    pub fn new(devices: &'a Mutex<DeviceStore<'r>>, clock: &'a dyn Clock, generation: u64) -> Self {
        Self {
            devices,
            clock,
            generation,
        }
    }
}
/// Holding this guard excludes device create/revoke until publication ends.
/// It never retains a password or allows callers to modify the registry.
pub struct DeviceGuard<'a, 'r> {
    _store: MutexGuard<'a, DeviceStore<'r>>,
    access: Access,
}
impl crate::store_fs::UploadGuard for DeviceGuard<'_, '_> {
    fn access(&self) -> Access {
        self.access
    }
}
impl<'r> crate::store_fs::UploadAuthorization for DevicePolicy<'_, 'r> {
    type Guard<'a>
        = DeviceGuard<'a, 'r>
    where
        Self: 'a;
    fn authorize(
        &self,
        account: AccountId,
        device: DeviceId,
        deadline: Deadline,
    ) -> Result<Self::Guard<'_>, Error> {
        let before = self.clock.sample()?.monotonic;
        if deadline.expired(before) {
            return Err(Error::Deadline);
        }
        let store = match self.devices.try_lock() {
            Ok(store) => store,
            Err(TryLockError::WouldBlock) => return Err(Error::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(Error::WriterStopped),
        };
        let active = store
            .active(account, device)
            .map_err(|_| Error::WriterStopped)?;
        let after = self.clock.sample()?.monotonic;
        if after.0 < before.0 {
            return Err(Error::Invalid);
        }
        if deadline.expired(after) {
            return Err(Error::Deadline);
        }
        if !active {
            return Err(Error::Forbidden);
        }
        Ok(DeviceGuard {
            _store: store,
            access: Access {
                account,
                principal: Principal::Device(device),
                config_generation: self.generation,
            },
        })
    }
}

impl<'r> crate::store_fs::UploadAuthorization for Option<DevicePolicy<'_, 'r>> {
    type Guard<'a>
        = DeviceGuard<'a, 'r>
    where
        Self: 'a;
    fn authorize(
        &self,
        account: AccountId,
        device: DeviceId,
        deadline: Deadline,
    ) -> Result<Self::Guard<'_>, Error> {
        self.as_ref()
            .ok_or(Error::Forbidden)?
            .authorize(account, device, deadline)
    }
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

// The personal-account policy grants reads and writes to active devices;
// blob ownership is checked separately by the read coordinator.
impl<'r> crate::store_fs::BlobAuthorization for DevicePolicy<'_, 'r> {
    type Guard<'a>
        = DeviceGuard<'a, 'r>
    where
        Self: 'a;
    fn authorize_blob(
        &self,
        account: AccountId,
        device: DeviceId,
        deadline: Deadline,
    ) -> Result<Self::Guard<'_>, Error> {
        crate::store_fs::UploadAuthorization::authorize(self, account, device, deadline)
    }
}
impl<'r> crate::store_fs::BlobAuthorization for Option<DevicePolicy<'_, 'r>> {
    type Guard<'a>
        = DeviceGuard<'a, 'r>
    where
        Self: 'a;
    fn authorize_blob(
        &self,
        account: AccountId,
        device: DeviceId,
        deadline: Deadline,
    ) -> Result<Self::Guard<'_>, Error> {
        self.as_ref()
            .ok_or(Error::Forbidden)?
            .authorize_blob(account, device, deadline)
    }
}

impl<'r> crate::store_fs::EmailUpdateAuthorization for DevicePolicy<'_, 'r> {
    type Guard<'a>
        = DeviceGuard<'a, 'r>
    where
        Self: 'a;
    fn authorize_email_update(
        &self,
        account: AccountId,
        device: DeviceId,
        deadline: Deadline,
    ) -> Result<Self::Guard<'_>, Error> {
        crate::store_fs::UploadAuthorization::authorize(self, account, device, deadline)
    }
}
impl<'r> crate::store_fs::EmailUpdateAuthorization for Option<DevicePolicy<'_, 'r>> {
    type Guard<'a>
        = DeviceGuard<'a, 'r>
    where
        Self: 'a;
    fn authorize_email_update(
        &self,
        account: AccountId,
        device: DeviceId,
        deadline: Deadline,
    ) -> Result<Self::Guard<'_>, Error> {
        self.as_ref()
            .ok_or(Error::Forbidden)?
            .authorize_email_update(account, device, deadline)
    }
}

impl<'r> crate::store_fs::MailboxAuthorization for DevicePolicy<'_, 'r> {
    type Guard<'a>
        = DeviceGuard<'a, 'r>
    where
        Self: 'a;
    fn authorize_mailboxes(
        &self,
        account: AccountId,
        device: DeviceId,
        deadline: Deadline,
    ) -> Result<Self::Guard<'_>, Error> {
        crate::store_fs::UploadAuthorization::authorize(self, account, device, deadline)
    }
}
impl<'r> crate::store_fs::MailboxAuthorization for Option<DevicePolicy<'_, 'r>> {
    type Guard<'a>
        = DeviceGuard<'a, 'r>
    where
        Self: 'a;
    fn authorize_mailboxes(
        &self,
        account: AccountId,
        device: DeviceId,
        deadline: Deadline,
    ) -> Result<Self::Guard<'_>, Error> {
        self.as_ref()
            .ok_or(Error::Forbidden)?
            .authorize_mailboxes(account, device, deadline)
    }
}

impl<'r> crate::store_fs::DraftAuthorization for DevicePolicy<'_, 'r> {
    type Guard<'a>
        = DeviceGuard<'a, 'r>
    where
        Self: 'a;
    fn authorize_draft(
        &self,
        account: AccountId,
        device: DeviceId,
        deadline: Deadline,
    ) -> Result<Self::Guard<'_>, Error> {
        crate::store_fs::UploadAuthorization::authorize(self, account, device, deadline)
    }
}
impl<'r> crate::store_fs::DraftAuthorization for Option<DevicePolicy<'_, 'r>> {
    type Guard<'a>
        = DeviceGuard<'a, 'r>
    where
        Self: 'a;
    fn authorize_draft(
        &self,
        account: AccountId,
        device: DeviceId,
        deadline: Deadline,
    ) -> Result<Self::Guard<'_>, Error> {
        self.as_ref()
            .ok_or(Error::Forbidden)?
            .authorize_draft(account, device, deadline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[allow(clippy::unwrap_used)]
    fn blob_policy_requires_a_live_registered_device_and_correct_account() {
        use crate::{
            ports::{Tick, Time},
            store_fs::{tests::Fixture, BlobAuthorization, UploadGuard},
        };
        struct Now;
        impl Clock for Now {
            fn sample(&self) -> Result<Time, Error> {
                Ok(Time {
                    utc_ms: 1,
                    monotonic: Tick(1),
                })
            }
        }
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let account = AccountId::from_bytes([1; 16]);
        let mut store = DeviceStore::open_or_create(&mut root, account).unwrap();
        let (device, _) = store
            .create(
                "reader",
                1,
                &mut td_crypto::SystemEntropy::try_new().unwrap(),
            )
            .unwrap();
        let devices = Mutex::new(store);
        let policy = Some(DevicePolicy::new(&devices, &Now, 7));
        let deadline = Deadline::after(Tick(1), 100).unwrap();
        use crate::store_fs::{DraftAuthorization, EmailUpdateAuthorization, MailboxAuthorization};
        let draft = policy.authorize_draft(account, device, deadline).unwrap();
        assert_eq!(draft.access().principal, Principal::Device(device));
        assert_eq!(draft.access().config_generation, 7);
        assert!(devices.try_lock().is_err());
        drop(draft);
        for (account, device) in [
            (account, DeviceId::from_bytes([99; 16])),
            (AccountId::from_bytes([99; 16]), device),
        ] {
            assert!(matches!(
                policy.authorize_draft(account, device, deadline),
                Err(Error::Forbidden)
            ));
        }
        let folders = policy
            .authorize_mailboxes(account, device, deadline)
            .unwrap();
        assert_eq!(folders.access().principal, Principal::Device(device));
        assert!(devices.try_lock().is_err());
        drop(folders);
        let flags = policy
            .authorize_email_update(account, device, deadline)
            .unwrap();
        assert_eq!(flags.access().principal, Principal::Device(device));
        assert!(devices.try_lock().is_err());
        drop(flags);
        let guard = policy.authorize_blob(account, device, deadline).unwrap();
        assert_eq!(guard.access().principal, Principal::Device(device));
        assert_eq!(guard.access().config_generation, 7);
        assert!(devices.try_lock().is_err());
        drop(guard);
        assert!(matches!(
            policy.authorize_blob(account, DeviceId::from_bytes([99; 16]), deadline),
            Err(Error::Forbidden)
        ));
        assert!(matches!(
            policy.authorize_blob(AccountId::from_bytes([99; 16]), device, deadline),
            Err(Error::Forbidden)
        ));
        devices.lock().unwrap().revoke(device, 2).unwrap();
        assert!(matches!(
            policy.authorize_draft(account, device, deadline),
            Err(Error::Forbidden)
        ));
        assert!(matches!(
            policy.authorize_blob(account, device, deadline),
            Err(Error::Forbidden)
        ));
        assert!(matches!(
            policy.authorize_email_update(account, device, deadline),
            Err(Error::Forbidden)
        ));
        assert!(matches!(
            policy.authorize_mailboxes(account, device, deadline),
            Err(Error::Forbidden)
        ));
        let absent: Option<DevicePolicy<'_, '_>> = None;
        assert!(matches!(
            absent.authorize_mailboxes(account, device, deadline),
            Err(Error::Forbidden)
        ));
        assert!(matches!(
            absent.authorize_email_update(account, device, deadline),
            Err(Error::Forbidden)
        ));
        assert!(matches!(
            absent.authorize_blob(account, device, deadline),
            Err(Error::Forbidden)
        ));
    }
    #[test]
    fn parsed_credentials_are_owned_bounded_and_not_authentication() {
        let header = String::from("bAsIc   dTpw");
        let credentials = parse_basic("u", Some(&header));
        drop(header);
        assert_eq!(
            credentials.as_ref().map(BasicCredentials::password),
            Some(&b"p"[..])
        );
        // Empty and malformed app passwords still require backend checking;
        // parsing must not turn their syntax into authentication evidence.
        assert_eq!(
            parse_basic("u", Some("Basic dTo="))
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
            assert!(parse_basic("u", header).is_none());
        }
        assert!(parse_basic("other", Some("Basic dTpw")).is_none());
        assert!(parse_basic("U", Some("Basic dTpw")).is_none());
        let oversized = format!("Basic {}", "dTpw".repeat(DECODED));
        assert!(parse_basic("u", Some(&oversized)).is_none());
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
