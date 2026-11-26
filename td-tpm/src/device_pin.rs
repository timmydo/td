//! A device-bound secret sealed with a PIN (DESIGN.md, "What belongs to
//! consumers"): td's dictionary-attack parameters and the check that the TPM
//! still holds them, the storage primary Name the boot selector proved,
//! and the PIN codec whose authValue such an object takes. td-protector,

use super::{zero, DaParameters, DictionaryAttack};
use std::fmt;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

/// td's dictionary-attack parameters, which the boot selector's lockout
/// take sets and its proof repairs: 32 tries, one more every 600 seconds,
/// and a day before a wrong lockout authorization may be tried again.
pub const DA_PARAMETERS: DaParameters = DaParameters {
    max_tries: 32,
    recovery_time: 600,
    lockout_recovery: 86400,
};

/// The storage primary Name a measured TPM-PIN release proved this boot,
/// in root's private admission directory (td-install/ENCRYPTION.md,
/// "Passkey storage primary").
pub const ADMITTED_PRIMARY: &str = "/run/td-admit/td-passkey-primary";
/// A SHA-256 TPM Name: the algorithm identifier, then the digest.
pub const PRIMARY_NAME_LEN: usize = 34;
/// The shortest PIN the codec admits.
pub const MIN_PIN_LEN: usize = 6;
/// The longest PIN the codec admits.
pub const MAX_PIN_LEN: usize = 63;
/// A device PIN object's salt, random per object and public beside it.
pub const SALT_LEN: usize = 32;
/// HMAC-SHA256's output, a SHA-256 object's largest authValue.
pub const AUTH_VALUE_LEN: usize = 32;

// open(2)'s flags differ by architecture: on aarch64 `0o400000` is
// O_LARGEFILE and O_NOFOLLOW is `0o100000`. The target is pinned whole, as
// td-fs pins it, so a wrong O_NOFOLLOW cannot become an open that follows
// a symlink.
#[cfg(not(all(
    any(target_arch = "x86_64", target_arch = "aarch64"),
    target_os = "linux"
)))]
compile_error!("td-tpm pins O_NONBLOCK/O_NOFOLLOW for x86_64-linux and aarch64-linux only");
const O_NONBLOCK: i32 = 0o4000;
#[cfg(target_arch = "x86_64")]
const O_NOFOLLOW: i32 = 0o400000;
#[cfg(target_arch = "aarch64")]
const O_NOFOLLOW: i32 = 0o100000;

impl DictionaryAttack {
    /// Whether the TPM holds td's policy: lockout authorization set, so
    /// no one without it can reset the counter, no lockout, a try left,
    /// and parameters no weaker than `DA_PARAMETERS`. A device PIN object
    /// is sealed or unsealed only when this holds.
    pub fn holds_td_policy(&self) -> bool {
        self.lockout_auth_set
            && !self.in_lockout
            && self.counter < self.max_tries
            && self.max_tries <= DA_PARAMETERS.max_tries
            && self.interval >= DA_PARAMETERS.recovery_time
            && self.recovery >= DA_PARAMETERS.lockout_recovery
    }
}

/// Why no admitted storage primary may be used. It carries no I/O
/// error: every caller refuses the same way whatever the cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionError {
    /// The Name could not be opened, inspected or read: this boot
    /// proved no primary.
    Unreadable,
    /// The file is not root's private, single-link Name of its size.
    NotPrivate,
    /// The Name is not a SHA-256 Name.
    NotSha256,
}

impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unreadable => "the admitted storage primary cannot be read",
            Self::NotPrivate => "the admitted storage primary is not root's private Name",
            Self::NotSha256 => "the admitted storage primary is not a SHA-256 Name",
        })
    }
}

/// The storage primary Name at `path`: a regular file owned by root,
/// mode 0600, one link, exactly a SHA-256 Name. Its absence means this
/// boot proved no primary, and no device PIN object may be used.
pub fn admitted_primary(path: &Path) -> Result<[u8; PRIMARY_NAME_LEN], AdmissionError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)
        .map_err(|_| AdmissionError::Unreadable)?;
    let meta = file.metadata().map_err(|_| AdmissionError::Unreadable)?;
    if !meta.is_file()
        || !private_name(
            meta.uid(),
            meta.gid(),
            meta.nlink(),
            meta.mode(),
            meta.len(),
        )
    {
        return Err(AdmissionError::NotPrivate);
    }
    let mut name = [0; PRIMARY_NAME_LEN];
    file.read_exact(&mut name)
        .map_err(|_| AdmissionError::Unreadable)?;
    if !sha256_name(&name) {
        return Err(AdmissionError::NotSha256);
    }
    Ok(name)
}

fn private_name(uid: u32, gid: u32, nlink: u64, mode: u32, len: u64) -> bool {
    (uid, gid, nlink, mode & 0o7777, len) == (0, 0, 1, 0o600, PRIMARY_NAME_LEN as u64)
}

fn sha256_name(name: &[u8; PRIMARY_NAME_LEN]) -> bool {
    name.get(..2) == Some(&[0, 11])
}

/// Why the codec refused an entry. Neither variant carries a byte of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinError {
    /// The entry is not `MIN_PIN_LEN` to `MAX_PIN_LEN` bytes long.
    Length { len: usize },
    /// The byte at this one-based offset is not printable ASCII (0x20 to
    /// 0x7e).
    Character { position: usize },
}
impl fmt::Display for PinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length { len } => write!(
                f,
                "a PIN has {MIN_PIN_LEN} to {MAX_PIN_LEN} bytes, not {len}"
            ),
            Self::Character { position } => {
                write!(f, "byte {position} of the PIN is not printable ASCII")
            }
        }
    }
}
impl std::error::Error for PinError {}

/// A device PIN as typed: 6 to 63 bytes, each 0x20 to 0x7e. It lives in
/// one heap allocation of exactly its length, zeroed on drop, and is
/// deliberately neither `Debug`, `Display` nor `Clone`.
pub struct Pin(Box<[u8]>);
impl Drop for Pin {
    fn drop(&mut self) {
        zero(&mut self.0);
    }
}
impl Pin {
    /// The only constructor. The length is judged first, then each byte;
    /// the caller zeroes `entry`, which is copied.
    pub fn parse(entry: &[u8]) -> Result<Self, PinError> {
        if !(MIN_PIN_LEN..=MAX_PIN_LEN).contains(&entry.len()) {
            return Err(PinError::Length { len: entry.len() });
        }
        if let Some(index) = entry.iter().position(|byte| !(0x20..=0x7e).contains(byte)) {
            return Err(PinError::Character {
                position: index + 1,
            });
        }
        Ok(Self(entry.into()))
    }

    /// The object's authValue under `salt` in `domain`: HMAC-SHA256 keyed
    /// with the salt over the domain, a zero byte and the PIN. Each kind of
    /// object names its own domain. All 32 bytes; removing trailing zero
    /// bytes where TPM 2.0 does is the session's.
    pub fn auth_value(&self, domain: &[u8], salt: &[u8; SALT_LEN]) -> AuthValue {
        let mut value = AuthValue(Box::new([0; AUTH_VALUE_LEN]));
        let mut mac = td_fido::hmac_sha256(salt, &[domain, &[0], &self.0]);
        value.0.copy_from_slice(&mac);
        zero(&mut mac);
        value
    }
}

/// A device PIN object's authValue, zeroed on drop and deliberately
/// neither `Debug`, `Display` nor `Clone`.
pub struct AuthValue(Box<[u8; AUTH_VALUE_LEN]>);
impl Drop for AuthValue {
    fn drop(&mut self) {
        zero(self.0.as_mut_slice());
    }
}
impl AuthValue {
    /// The bytes, lent to the TPM command that seals or unseals with them.
    pub fn expose(&self) -> &[u8; AUTH_VALUE_LEN] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attack(f: impl FnOnce(&mut DictionaryAttack)) -> DictionaryAttack {
        let mut da = DictionaryAttack {
            lockout_auth_set: true,
            in_lockout: false,
            counter: 0,
            max_tries: 32,
            interval: 600,
            recovery: 86400,
        };
        f(&mut da);
        da
    }

    #[test]
    fn only_td_s_dictionary_attack_policy_admits_a_device_pin() {
        assert!(attack(|_| ()).holds_td_policy());
        assert!(attack(|d| d.max_tries = 8).holds_td_policy());
        assert!(!attack(|d| d.lockout_auth_set = false).holds_td_policy());
        assert!(!attack(|d| d.in_lockout = true).holds_td_policy());
        assert!(!attack(|d| d.counter = 32).holds_td_policy());
        assert!(!attack(|d| d.max_tries = 33).holds_td_policy());
        assert!(!attack(|d| d.interval = 599).holds_td_policy());
        assert!(!attack(|d| d.recovery = 86399).holds_td_policy());
    }

    #[test]
    fn the_admitted_primary_is_read_from_a_real_private_file_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_temp::TempDir::new("td-tpm-primary").unwrap();
        let mut name = [9; PRIMARY_NAME_LEN];
        name[..2].copy_from_slice(&[0, 11]);
        let private = |file: &str, bytes: &[u8], mode: u32| {
            let path = dir.path().join(file);
            std::fs::write(&path, bytes).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path
        };
        let file = private("primary", &name, 0o600);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        // A symlink is never followed, even to a well-formed Name.
        assert_eq!(admitted_primary(&link), Err(AdmissionError::Unreadable));
        // A directory is not a regular file.
        assert_eq!(
            admitted_primary(dir.path()),
            Err(AdmissionError::NotPrivate)
        );
        let short = private("short", &name[..33], 0o600);
        assert_eq!(admitted_primary(&short), Err(AdmissionError::NotPrivate));
        let open = private("open", &name, 0o644);
        assert_eq!(admitted_primary(&open), Err(AdmissionError::NotPrivate));
        // Only root owns an admitted Name, uid and gid 0. The file is this
        // process's, so its owner is the process's; a build sandbox may
        // run as uid 0 with another group, where acceptance is not shown.
        let meta = std::fs::metadata(&file).unwrap();
        match (meta.uid(), meta.gid()) {
            (0, 0) => {
                assert_eq!(admitted_primary(&file), Ok(name));
                let other = private("other", &[7; PRIMARY_NAME_LEN], 0o600);
                assert_eq!(admitted_primary(&other), Err(AdmissionError::NotSha256));
            }
            (0, _) => (),
            _ => assert_eq!(admitted_primary(&file), Err(AdmissionError::NotPrivate)),
        }
    }

    #[test]
    fn the_admitted_primary_must_be_root_s_private_sha256_name() {
        assert_eq!(
            admitted_primary(Path::new("/nonexistent/td-passkey-primary")),
            Err(AdmissionError::Unreadable)
        );
        assert!(private_name(0, 0, 1, 0o100600, 34));
        assert!(!private_name(1000, 0, 1, 0o600, 34));
        assert!(!private_name(0, 1, 1, 0o600, 34));
        assert!(!private_name(0, 0, 2, 0o600, 34));
        assert!(!private_name(0, 0, 1, 0o640, 34));
        assert!(!private_name(0, 0, 1, 0o600, 33));
        let mut name = [7; PRIMARY_NAME_LEN];
        assert!(!sha256_name(&name));
        name[..2].copy_from_slice(&[0, 11]);
        assert!(sha256_name(&name));
    }

    #[test]
    fn a_pin_is_6_to_63_printable_bytes_and_its_auth_value_is_domain_bound() {
        assert_eq!(
            Pin::parse(b"12345").err(),
            Some(PinError::Length { len: 5 })
        );
        assert_eq!(
            Pin::parse(&[b'a'; 64]).err(),
            Some(PinError::Length { len: 64 })
        );
        assert_eq!(
            Pin::parse(b"12345\n").err(),
            Some(PinError::Character { position: 6 })
        );
        let pin = Pin::parse(b"123456").unwrap();
        let a = pin.auth_value(b"one", &[1; 32]);
        let b = pin.auth_value(b"two", &[1; 32]);
        let c = pin.auth_value(b"one", &[2; 32]);
        assert_ne!(a.expose(), b.expose());
        assert_ne!(a.expose(), c.expose());
        assert_eq!(
            a.expose(),
            &td_fido::hmac_sha256(&[1; 32], &[b"one", &[0], b"123456"])
        );
    }
}
