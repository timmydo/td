//! The protected tier's TPM PIN (DESIGN.md "PIN and tpm-pin policy"): the
//! PIN codec, the authValue a PIN gives a tpm-pin object, the tpm-pin
//! policy, and the chain check that answers whether a PIN may be asked.
//! Library code: nothing in production calls it before ENCRYPTION.md
//! increment 8c.

use crate::{CAP_PCR, SELECTOR_IMAGE_PCR, SELECTOR_INITRD_PCR};
use std::fmt;
use td_tpm::{put32, Client, PcrReadError, PcrSelection, Reader, Refusal, SealedObject, Transport};

/// The shortest PIN the codec admits.
pub const MIN_PIN_LEN: usize = 6;
/// The longest PIN the codec admits.
pub const MAX_PIN_LEN: usize = 63;
/// The HMAC message prefix of a PIN's authValue; a zero byte follows it.
pub const PIN_DOMAIN: &[u8] = b"td/disk-protector/pin/v1";
/// A tpm-pin object's salt, random per object and public in its token.
pub const SALT_LEN: usize = 32;
/// HMAC-SHA256's output, a SHA-256 object's largest authValue.
pub const AUTH_VALUE_LEN: usize = 32;
/// A SHA-256 TPM Name: the algorithm identifier, then the digest.
pub const PRIMARY_NAME_LEN: usize = 34;
/// The firmware's Secure Boot state, which a seal names only when td-boot
/// finds Secure Boot enabled in the measured boot.
pub const SECURE_BOOT_PCR: u8 = 7;
/// A tpm-pin policy's selection without PCR 7.
pub const TPM_PIN_PCRS: &[u8] = &[SELECTOR_IMAGE_PCR, SELECTOR_INITRD_PCR, CAP_PCR];
/// A tpm-pin policy's selection naming PCR 7.
pub const TPM_PIN_SECURE_BOOT_PCRS: &[u8] = &[
    SELECTOR_IMAGE_PCR,
    SECURE_BOOT_PCR,
    SELECTOR_INITRD_PCR,
    CAP_PCR,
];
/// TPM_CC_PolicyAuthValue.
pub const POLICY_AUTH_VALUE: u32 = 0x16b;
/// fixedTPM | fixedParent | adminWithPolicy, with userWithAuth and noDA
/// clear: the policy, which includes the PIN, is the only release path,
/// and a wrong PIN counts against the TPM's lockout.
pub const PROTECTED_ATTRIBUTES: u32 = 0x92;
const KEYED_HASH: u16 = 0x0008;

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

/// A TPM PIN as typed: 6 to 63 bytes, each 0x20 to 0x7e. It lives in one
/// heap allocation of exactly its length, zeroed on drop, and is
/// deliberately neither `Debug`, `Display` nor `Clone`.
pub struct Pin(Box<[u8]>);
impl Drop for Pin {
    fn drop(&mut self) {
        td_tpm::zero(&mut self.0);
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

    /// The tpm-pin object's authValue under `salt`: HMAC-SHA256 keyed with
    /// the salt over `PIN_DOMAIN`, a zero byte and the PIN. All 32 bytes;
    /// removing trailing zero bytes where TPM 2.0 does is td-tpm's.
    pub fn auth_value(&self, salt: &[u8; SALT_LEN]) -> AuthValue {
        let mut value = AuthValue(Box::new([0; AUTH_VALUE_LEN]));
        let mut mac = hmac_sha256(salt, &[PIN_DOMAIN, &[0], &self.0]);
        value.0.copy_from_slice(&mac);
        td_tpm::zero(&mut mac);
        value
    }
}

/// A tpm-pin object's authValue, zeroed on drop and deliberately neither
/// `Debug`, `Display` nor `Clone`.
pub struct AuthValue(Box<[u8; AUTH_VALUE_LEN]>);
impl Drop for AuthValue {
    fn drop(&mut self) {
        td_tpm::zero(self.0.as_mut_slice());
    }
}
impl AuthValue {
    /// The bytes, lent to the TPM command that seals or unseals with them.
    pub fn expose(&self) -> &[u8; AUTH_VALUE_LEN] {
        &self.0
    }
}

/// The engine's SHA-256, which td-fido's `hmac.rs` names as
/// `super::sha256`.
#[path = "../../engine/src/sha256.rs"]
#[allow(
    dead_code,
    reason = "the shared hash also supports build artifact files"
)]
mod sha256;

/// td-fido's HMAC-SHA256, compiled by path as td-secret's store crypto
/// compiles it (td-fido/DESIGN.md, "Shared HMAC-SHA256"). It streams the
/// parts, so the PIN is never copied into a hash input buffer.
#[path = "../../td-fido/src/hmac.rs"]
#[allow(dead_code, reason = "the PIN derivation calls `hmac_sha256` alone")]
mod hmac;

use hmac::hmac_sha256;

/// Which PCRs a tpm-pin policy names besides the literal-zero PCR 12: the
/// token's `pcrs`, `4,9` or `4,7,9`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinPcrs {
    /// PCRs 4 and 9.
    Selector,
    /// PCRs 4, 7 and 9.
    SecureBoot,
}
impl PinPcrs {
    /// The policy's whole selection, ascending, PCR 12 last.
    pub fn pcrs(self) -> &'static [u8] {
        match self {
            Self::Selector => TPM_PIN_PCRS,
            Self::SecureBoot => TPM_PIN_SECURE_BOOT_PCRS,
        }
    }

    /// The PCRs whose values the policy takes from a seal or a read: every
    /// one but PCR 12.
    pub fn measured(self) -> &'static [u8] {
        self.pcrs()
            .split_last()
            .map_or(&[][..], |(_, measured)| measured)
    }
}

/// A tpm-pin policy: PolicyPCR over the SHA-256 bank, then
/// PolicyAuthValue, then PolicyCommandCode(Unseal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinPolicy {
    pcrs: PinPcrs,
    selection: PcrSelection,
    pcr_digest: [u8; 32],
}
impl PinPolicy {
    /// The policy over `values`, those of `pcrs.measured()` in ascending
    /// PCR order, and a literal-zero PCR 12.
    pub fn new(pcrs: PinPcrs, values: &[[u8; 32]]) -> Result<Self, String> {
        if values.len() != pcrs.measured().len() {
            return Err("a tpm-pin policy takes one value per measured PCR".into());
        }
        let mut composite = values.to_vec();
        composite.push([0; 32]);
        Ok(Self {
            pcrs,
            selection: crate::selection(pcrs.pcrs())?,
            pcr_digest: td_tpm::pcr_digest(&composite),
        })
    }

    pub fn pcrs(&self) -> PinPcrs {
        self.pcrs
    }

    pub fn selection(&self) -> PcrSelection {
        self.selection
    }

    /// The composite PolicyPCR compares.
    pub fn pcr_digest(&self) -> [u8; 32] {
        self.pcr_digest
    }

    /// The policy digest a tpm-pin object carries as its authPolicy, each
    /// step extended from the zero digest as TPM 2.0 Part 3 specifies.
    pub fn digest(&self) -> [u8; 32] {
        let mut bytes = vec![0; 32];
        put32(&mut bytes, td_tpm::POLICY_PCR);
        bytes.extend_from_slice(&self.selection.marshal());
        bytes.extend_from_slice(&self.pcr_digest);
        let mut bytes = td_tpm::digest(&bytes).to_vec();
        put32(&mut bytes, POLICY_AUTH_VALUE);
        let mut bytes = td_tpm::digest(&bytes).to_vec();
        put32(&mut bytes, td_tpm::POLICY_COMMAND_CODE);
        put32(&mut bytes, td_tpm::UNSEAL);
        td_tpm::digest(&bytes)
    }
}

/// The tpm-pin policy over `pcrs`' measured PCRs as they read now, in one
/// PCR_Read, measured or not, and a literal-zero PCR 12, which is never
/// read. A TPM without a SHA-256 bank is typed as td-tpm types it.
pub fn release_pin_policy<T: Transport>(
    client: &mut Client<T>,
    pcrs: PinPcrs,
) -> Result<PinPolicy, PcrReadError> {
    let values = client.read_pcrs_typed(crate::selection(pcrs.measured())?)?;
    Ok(PinPolicy::new(pcrs, &values)?)
}

/// The authPolicy of a tpm-pin object's public area: a SHA-256 keyed-hash
/// object with `PROTECTED_ATTRIBUTES`, a 32-byte authPolicy, no scheme and
/// a 32-byte unique digest, refusing any other area.
pub fn protected_auth_policy(public: &[u8]) -> Result<[u8; 32], String> {
    const NOT_PROTECTED: &str = "sealed TPM object is not a tpm-pin object";
    let mut reader = Reader(public);
    if reader.u16()? != KEYED_HASH
        || reader.u16()? != td_tpm::SHA256
        || reader.u32()? != PROTECTED_ATTRIBUTES
    {
        return Err(NOT_PROTECTED.into());
    }
    let policy = reader.blob()?.try_into().map_err(|_| NOT_PROTECTED)?;
    if reader.u16()? != td_tpm::ALG_NULL || reader.blob()?.len() != 32 {
        return Err(NOT_PROTECTED.into());
    }
    reader.end()?;
    Ok(policy)
}

/// Why the chain check asks for no PIN (ENCRYPTION.md "Protected
/// release", the chain check and its warning).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainError {
    /// The sealed public area is not a tpm-pin object's; nothing was sent.
    NotProtected(String),
    /// The PCR_Read found no SHA-256 bank: no SHA-256 PolicyPCR can be met.
    NoSha256Bank,
    /// The TPM refused CreatePrimary of the owner storage primary: an owner
    /// hierarchy given an authorization or disabled since the seal
    /// (`HIERARCHY_REFUSED`), or another refusal it names.
    PrimaryRefused(Refusal),
    /// The storage primary's Name is not the token's: a cleared or
    /// replaced TPM, or an interposer that was absent at the seal. The
    /// object was not loaded.
    OtherPrimary,
    /// The TPM refused to load the object under a primary of the token's
    /// Name: its private area does not verify there.
    LoadRefused(Refusal),
    /// The TPM loads the object, but the policy over the PCRs as they read
    /// now is not its authPolicy: the boot chain changed.
    ChangedChain,
    /// A transport error, a reply that does not answer its command, a PCR
    /// read the TPM refused, or a flush that failed after the Load.
    Other(String),
}
impl fmt::Display for ChainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotProtected(error) => f.write_str(error),
            Self::NoSha256Bank => {
                f.write_str("TPM has no SHA-256 PCR bank: no protector policy can be met")
            }
            Self::PrimaryRefused(refusal) => write!(
                f,
                "the TPM refused its storage primary: TPM command {:#x} refused: {:#x}",
                refusal.command, refusal.rc
            ),
            Self::OtherPrimary => {
                f.write_str("the TPM's storage primary is not the one the protector names")
            }
            Self::LoadRefused(refusal) => write!(
                f,
                "the TPM does not load the protector: TPM command {:#x} refused: {:#x}",
                refusal.command, refusal.rc
            ),
            Self::ChangedChain => {
                f.write_str("the boot chain is not the one the protector was sealed to")
            }
            Self::Other(error) => f.write_str(error),
        }
    }
}
impl std::error::Error for ChainError {}
impl From<PcrReadError> for ChainError {
    fn from(error: PcrReadError) -> Self {
        match error {
            PcrReadError::NoSha256Bank => Self::NoSha256Bank,
            PcrReadError::Other(error) => Self::Other(error),
        }
    }
}

/// Whether a tpm-pin object can release here, asking for nothing and
/// costing no attempt: the policy over `pcrs` as they read now and a
/// literal-zero PCR 12 must equal the sealed authPolicy, the storage
/// primary's Name must equal `primary`, the token's, and the TPM must load
/// the object under it, which verifies its private area; the object and
/// the primary are then flushed. No authorization is sent. The Name is
/// compared before the Load, which a primary of another Name is not sent;
/// the Load runs whatever the digest, so that a changed chain is told from
/// an object the TPM will not load. A TPM refusal is typed by the command
/// it refused, through td-tpm's `last_refusal`; anything else is `Other`.
/// `Ok` means the PIN may be asked. The client is consumed, so a handle a
/// failure leaves is flushed when it drops.
pub fn chain_check<T: Transport>(
    mut client: Client<T>,
    pcrs: PinPcrs,
    primary: &[u8; PRIMARY_NAME_LEN],
    sealed: &SealedObject,
) -> Result<(), ChainError> {
    let sealed_policy = protected_auth_policy(&sealed.public).map_err(ChainError::NotProtected)?;
    let policy = release_pin_policy(&mut client, pcrs)?;
    let refused = |client: &Client<T>, command: u32, error: String| {
        client
            .last_refusal()
            .filter(|refusal| refusal.command == command)
            .ok_or(ChainError::Other(error))
    };
    let (parent, name) = match client.storage_primary(None) {
        Ok(primary) => primary,
        Err(error) => {
            let refusal = refused(&client, td_tpm::CREATE_PRIMARY, error)?;
            return Err(ChainError::PrimaryRefused(refusal));
        }
    };
    if name != primary {
        client.flush(parent).map_err(ChainError::Other)?;
        return Err(ChainError::OtherPrimary);
    }
    let object = match client.load(parent, &sealed.public, &sealed.private) {
        Ok(object) => object,
        Err(error) => {
            let refusal = refused(&client, td_tpm::LOAD, error)?;
            return Err(ChainError::LoadRefused(refusal));
        }
    };
    client.flush(object).map_err(ChainError::Other)?;
    client.flush(parent).map_err(ChainError::Other)?;
    if policy.digest() != sealed_policy {
        return Err(ChainError::ChangedChain);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Scripted;
    use td_tpm::{put16, put_blob, ALG_NULL, CREATE_PRIMARY, FLUSH_CONTEXT, LOAD, PCR_READ};

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn parsed(entry: &[u8]) -> Pin {
        match Pin::parse(entry) {
            Ok(pin) => pin,
            Err(error) => panic!("refused {entry:?}: {error}"),
        }
    }

    fn refused(entry: &[u8]) -> PinError {
        match Pin::parse(entry) {
            Ok(_) => panic!("admitted {entry:?}"),
            Err(error) => error,
        }
    }

    #[test]
    fn the_codec_admits_6_to_63_printable_ascii_bytes_space_included() {
        assert_eq!((MIN_PIN_LEN, MAX_PIN_LEN), (6, 63));
        assert_eq!(refused(b""), PinError::Length { len: 0 });
        assert_eq!(refused(b"12345"), PinError::Length { len: 5 });
        assert_eq!(parsed(b"123456").0.as_ref(), b"123456");
        let longest = [b'~'; 63];
        assert_eq!(parsed(&longest).0.as_ref(), longest);
        assert_eq!(refused(&[b'7'; 64]), PinError::Length { len: 64 });
        // The length is judged before the bytes.
        assert_eq!(refused(b"\x1f\x7f\x00"), PinError::Length { len: 3 });
        assert_eq!(refused(&[0; 64]), PinError::Length { len: 64 });
        // Space admitted, at either end and inside.
        assert_eq!(parsed(b"pass phrase").0.as_ref(), b"pass phrase");
        assert_eq!(parsed(b" 12345").0.as_ref(), b" 12345");
        assert_eq!(parsed(b"12345 ").0.as_ref(), b"12345 ");
        assert_eq!(parsed(b"      ").0.as_ref(), b"      ");
        // Every printable byte is admitted and every other refused, at its
        // one-based offset.
        for byte in 0..=u8::MAX {
            let mut entry = *b"abcdefg";
            entry[3] = byte;
            if (0x20..=0x7e).contains(&byte) {
                assert_eq!(parsed(&entry).0.as_ref(), entry);
            } else {
                assert_eq!(refused(&entry), PinError::Character { position: 4 });
            }
        }
        assert_eq!(refused(b"\x1f23456"), PinError::Character { position: 1 });
        assert_eq!(refused(b"12345\x7f"), PinError::Character { position: 6 });
        // A multi-byte character is refused at its first byte.
        assert_eq!(
            refused("12ä456".as_bytes()),
            PinError::Character { position: 3 }
        );
        // The refusals name the offset or length and no byte.
        assert_eq!(
            PinError::Length { len: 5 }.to_string(),
            "a PIN has 6 to 63 bytes, not 5"
        );
        assert_eq!(
            PinError::Character { position: 6 }.to_string(),
            "byte 6 of the PIN is not printable ASCII"
        );
    }

    /// The authValue is a persisted contract: a tpm-pin object carries it.
    /// The literals are `tests/pin_vectors.py`'s, from Python's hmac.
    #[test]
    fn the_auth_value_matches_independent_vectors() {
        assert_eq!(PIN_DOMAIN, b"td/disk-protector/pin/v1");
        let salt: [u8; 32] = std::array::from_fn(|index| index as u8);
        let printable: Vec<u8> = (0x20..0x5f).collect();
        assert_eq!(printable.len(), MAX_PIN_LEN);
        for (pin, expected) in [
            (
                &b"123456"[..],
                "bce332ef49691954be77626859c32c787af8b67a2dc283c8f552d7672041132d",
            ),
            (
                &b"pass phrase"[..],
                "6c2be679349298f20dc5e50d326e5fcfe88d5ef0684079b44086629ac59d2422",
            ),
            (
                &printable[..],
                "ef54d4ed42e93719374c0d8a9b2cc2bcad1dba9e7624266ad5b85d11d2cbdfc4",
            ),
        ] {
            assert_eq!(hex(parsed(pin).auth_value(&salt).expose()), expected);
        }
        assert_eq!(
            hex(parsed(b"123456").auth_value(&[0xa5; 32]).expose()),
            "62f1c44be5b7c833928c514cefdabf97bad78bb6832e187098c376656b783c7f"
        );
    }

    /// The policy digest is a persisted contract: a tpm-pin object carries
    /// it as its authPolicy. The literals are `tests/pin_vectors.py`'s,
    /// over PCRs 4, 7 and 9 at [0x44; 32], [0x47; 32] and [0x49; 32].
    #[test]
    fn the_policy_digest_matches_independent_vectors() {
        assert_eq!(POLICY_AUTH_VALUE, 0x16b);
        assert_eq!(PROTECTED_ATTRIBUTES, 0x2 | 0x10 | 0x80);
        assert_eq!(PinPcrs::Selector.pcrs(), [4, 9, 12]);
        assert_eq!(PinPcrs::Selector.measured(), [4, 9]);
        assert_eq!(PinPcrs::SecureBoot.pcrs(), [4, 7, 9, 12]);
        assert_eq!(PinPcrs::SecureBoot.measured(), [4, 7, 9]);
        let selector = PinPolicy::new(PinPcrs::Selector, &[[0x44; 32], [0x49; 32]]).unwrap();
        assert_eq!(hex(&selector.selection().marshal()), "00000001000b03101200");
        assert_eq!(
            hex(&selector.pcr_digest()),
            "f627edaf23ffe7e6ee92093cfe85018c5cf854bad1e62e9709fe4eff5354bca5"
        );
        assert_eq!(
            hex(&selector.digest()),
            "81c76bdb2452e876666d16af17349d0e456703d57e6f2c73ca60d98f1c7d79c9"
        );
        let secure_boot =
            PinPolicy::new(PinPcrs::SecureBoot, &[[0x44; 32], [0x47; 32], [0x49; 32]]).unwrap();
        assert_eq!(
            hex(&secure_boot.selection().marshal()),
            "00000001000b03901200"
        );
        assert_eq!(
            hex(&secure_boot.pcr_digest()),
            "b9df80a1ad1a1da03a74e8d5b66df839a06be3512577dad13e072a6618bc4c4b"
        );
        assert_eq!(
            hex(&secure_boot.digest()),
            "6aa4fd0a4446e73061ac0f7624582d8485a6915e9676435acbcd5b5b2abff7df"
        );
        assert_eq!(secure_boot.pcrs(), PinPcrs::SecureBoot);
        // Not the device-bound policy over the same values.
        assert_ne!(
            selector.digest(),
            td_tpm::policy_digest(selector.selection(), &selector.pcr_digest())
        );
        assert!(PinPolicy::new(PinPcrs::Selector, &[[0x44; 32]]).is_err());
        assert!(PinPolicy::new(PinPcrs::SecureBoot, &[[0x44; 32], [0x49; 32]]).is_err());
    }

    /// A tpm-pin object's public area as td-tpm's seal will write it, with
    /// `policy` as its authPolicy; the scripted TPM loads a private area
    /// carrying its prefix.
    fn object(attributes: u32, policy: &[u8; 32]) -> SealedObject {
        let mut public = Vec::new();
        put16(&mut public, KEYED_HASH);
        put16(&mut public, td_tpm::SHA256);
        put32(&mut public, attributes);
        put_blob(&mut public, policy).unwrap();
        put16(&mut public, ALG_NULL);
        put_blob(&mut public, &[0x33; 32]).unwrap();
        SealedObject {
            public,
            private: b"scripted:payload".to_vec(),
        }
    }

    fn primary(tpm: &Scripted) -> [u8; PRIMARY_NAME_LEN] {
        let (_, name) = tpm.client().storage_primary(None).unwrap();
        name.try_into().unwrap()
    }

    #[test]
    fn the_chain_check_reads_compares_and_loads_sending_no_authorization() {
        for pcrs in [PinPcrs::Selector, PinPcrs::SecureBoot] {
            let tpm = Scripted::new();
            tpm.0.borrow_mut().pcrs[7] = [0x47; 32];
            let name = primary(&tpm);
            let policy = release_pin_policy(&mut tpm.client(), pcrs).unwrap();
            let sealed = object(PROTECTED_ATTRIBUTES, &policy.digest());
            tpm.codes();
            chain_check(tpm.client(), pcrs, &name, &sealed).unwrap();
            assert_eq!(
                tpm.codes(),
                [PCR_READ, CREATE_PRIMARY, LOAD, FLUSH_CONTEXT, FLUSH_CONTEXT]
            );
            assert!(tpm.idle());
            // PCR 12 entered the policy as a literal and was never read,
            // so the check still passes once the cap has run.
            crate::cap(&mut tpm.client()).unwrap();
            tpm.codes();
            chain_check(tpm.client(), pcrs, &name, &sealed).unwrap();
            assert_eq!(tpm.codes()[0], PCR_READ);

            // A changed PCR the token names: the TPM loads the object, and
            // its authPolicy is not the policy over the PCRs now.
            for changed in pcrs.measured() {
                let tpm = Scripted::new();
                tpm.0.borrow_mut().pcrs[7] = [0x47; 32];
                tpm.0.borrow_mut().pcrs[usize::from(*changed)] = [0x50; 32];
                assert_eq!(
                    chain_check(tpm.client(), pcrs, &name, &sealed),
                    Err(ChainError::ChangedChain)
                );
                assert!(tpm.codes().contains(&LOAD));
                assert!(tpm.idle());
            }
        }

        let tpm = Scripted::new();
        let name = primary(&tpm);
        let policy = release_pin_policy(&mut tpm.client(), PinPcrs::Selector).unwrap();
        let sealed = object(PROTECTED_ATTRIBUTES, &policy.digest());
        // PCR 7 is outside a 4,9 token's policy.
        tpm.0.borrow_mut().pcrs[7] = [0x51; 32];
        chain_check(tpm.client(), PinPcrs::Selector, &name, &sealed).unwrap();
        // A token naming PCR 7 does not pass an object sealed without it.
        assert_eq!(
            chain_check(tpm.client(), PinPcrs::SecureBoot, &name, &sealed),
            Err(ChainError::ChangedChain)
        );
        tpm.0.borrow_mut().pcrs[7] = [0; 32];

        // Another primary's Name: nothing is loaded.
        let mut other = name;
        other[PRIMARY_NAME_LEN - 1] ^= 1;
        tpm.codes();
        assert_eq!(
            chain_check(tpm.client(), PinPcrs::Selector, &other, &sealed),
            Err(ChainError::OtherPrimary)
        );
        assert_eq!(tpm.codes(), [PCR_READ, CREATE_PRIMARY, FLUSH_CONTEXT]);
        assert!(tpm.idle());

        // A cleared TPM's Load refusal, and a private area that does not
        // verify, whatever the digest.
        let integrity = Refusal {
            command: LOAD,
            rc: 0x1df,
        };
        tpm.0.borrow_mut().cleared = true;
        let cleared = chain_check(tpm.client(), PinPcrs::Selector, &name, &sealed);
        assert_eq!(cleared, Err(ChainError::LoadRefused(integrity)));
        assert_eq!(
            cleared.unwrap_err().to_string(),
            "the TPM does not load the protector: TPM command 0x157 refused: 0x1df"
        );
        assert!(tpm.idle());
        tpm.0.borrow_mut().cleared = false;
        let mut forged = sealed.clone();
        forged.private = b"forged".to_vec();
        tpm.0.borrow_mut().pcrs[4] = [0x50; 32];
        assert_eq!(
            chain_check(tpm.client(), PinPcrs::Selector, &name, &forged),
            Err(ChainError::LoadRefused(integrity))
        );
        tpm.0.borrow_mut().pcrs[4] = [0x44; 32];
        // A refused primary is no Load, and no Load refusal: an owner
        // authorization set since the seal, and a disabled hierarchy.
        for rc in [0x9a2, 0x185] {
            tpm.0.borrow_mut().refuse = Some((CREATE_PRIMARY, rc));
            tpm.codes();
            assert_eq!(
                chain_check(tpm.client(), PinPcrs::Selector, &name, &sealed),
                Err(ChainError::PrimaryRefused(Refusal {
                    command: CREATE_PRIMARY,
                    rc
                }))
            );
            assert_eq!(tpm.codes(), [PCR_READ, CREATE_PRIMARY]);
        }
        tpm.0.borrow_mut().refuse = None;
        // A flush that fails after the TPM loaded the object is no Load
        // refusal.
        let tpm = Scripted::new();
        tpm.0.borrow_mut().refuse = Some((FLUSH_CONTEXT, 0x18b));
        assert_eq!(
            chain_check(tpm.client(), PinPcrs::Selector, &name, &sealed),
            Err(ChainError::Other("TPM command 0x165 refused: 0x18b".into()))
        );
        assert_eq!(
            tpm.codes()[..4],
            [PCR_READ, CREATE_PRIMARY, LOAD, FLUSH_CONTEXT]
        );
        // Nor is a lost reply to the primary or the Load.
        for (lost, sent) in [
            (2, &[PCR_READ, CREATE_PRIMARY][..]),
            (3, &[PCR_READ, CREATE_PRIMARY, LOAD]),
        ] {
            let tpm = Scripted::new();
            tpm.0.borrow_mut().lose = vec![lost];
            assert_eq!(
                chain_check(tpm.client(), PinPcrs::Selector, &name, &sealed),
                Err(ChainError::Other("lost reply".into()))
            );
            assert_eq!(tpm.codes()[..lost], *sent);
        }
        let tpm = Scripted::new();

        // A TPM without a SHA-256 bank: the read alone.
        tpm.0.borrow_mut().no_sha256_bank = true;
        tpm.codes();
        assert_eq!(
            chain_check(tpm.client(), PinPcrs::Selector, &name, &sealed),
            Err(ChainError::NoSha256Bank)
        );
        assert_eq!(tpm.codes(), [PCR_READ]);
        tpm.0.borrow_mut().no_sha256_bank = false;
    }

    /// Only a tpm-pin object's public area is checked: one with noDA or
    /// userWithAuth set, the device-bound attributes, another type, scheme
    /// or field length is refused before any TPM command.
    #[test]
    fn the_chain_check_refuses_another_object_format_before_any_command() {
        let tpm = Scripted::new();
        let name = primary(&tpm);
        let policy = release_pin_policy(&mut tpm.client(), PinPcrs::Selector)
            .unwrap()
            .digest();
        let good = object(PROTECTED_ATTRIBUTES, &policy);
        assert_eq!(protected_auth_policy(&good.public), Ok(policy));
        let mut forms = vec![
            object(PROTECTED_ATTRIBUTES | 0x400, &policy),
            object(PROTECTED_ATTRIBUTES | 0x40, &policy),
            object(td_tpm::SEALED_ATTRIBUTES, &policy),
            object(PROTECTED_ATTRIBUTES & !0x80, &policy),
        ];
        for (at, value) in [(1, 0x23), (3, 0x0c), (43, 0x0b)] {
            let mut public = good.clone();
            public.public[at] = value;
            forms.push(public);
        }
        let mut short_policy = good.clone();
        short_policy.public[9] = 31;
        forms.push(short_policy);
        let mut trailing = good.clone();
        trailing.public.push(0);
        forms.push(trailing);
        let mut truncated = good.clone();
        truncated.public.pop();
        forms.push(truncated);
        tpm.codes();
        for sealed in forms {
            assert!(matches!(
                chain_check(tpm.client(), PinPcrs::Selector, &name, &sealed),
                Err(ChainError::NotProtected(_))
            ));
        }
        assert!(tpm.codes().is_empty());
    }

    mod emulator {
        use super::*;
        use crate::tests::emulator::{fresh_state, Emulator};
        use td_tpm::{CREATE, NULL, PASSWORD, POLICY_GET_DIGEST, START_AUTH_SESSION};

        /// The TPM's own digest of the tpm-pin policy: in a trial session
        /// over the policy's composite, or in a real policy session with an
        /// empty pcrDigest, for which the TPM takes the composite of the
        /// selected PCRs as they are.
        fn session_digest<T: Transport>(
            client: &mut Client<T>,
            policy: &PinPolicy,
            trial: bool,
        ) -> Vec<u8> {
            let mut start = Vec::new();
            put_blob(&mut start, &[0x11; 32]).unwrap();
            put_blob(&mut start, &[]).unwrap();
            start.push(if trial { 0x03 } else { 0x01 });
            put16(&mut start, ALG_NULL);
            put16(&mut start, td_tpm::SHA256);
            let (session, _) = client
                .call(START_AUTH_SESSION, &[NULL, NULL], None, &start, true)
                .unwrap();
            let session = session.unwrap();
            let mut pcr = Vec::new();
            let composite = policy.pcr_digest();
            put_blob(&mut pcr, if trial { &composite[..] } else { &[] }).unwrap();
            pcr.extend_from_slice(&policy.selection().marshal());
            client
                .call(td_tpm::POLICY_PCR, &[session], None, &pcr, false)
                .unwrap();
            client
                .call(POLICY_AUTH_VALUE, &[session], None, &[], false)
                .unwrap();
            client
                .call(
                    td_tpm::POLICY_COMMAND_CODE,
                    &[session],
                    None,
                    &td_tpm::UNSEAL.to_be_bytes(),
                    false,
                )
                .unwrap();
            let (_, out) = client
                .call(POLICY_GET_DIGEST, &[session], None, &[], false)
                .unwrap();
            client.flush(session).unwrap();
            let mut reader = Reader(&out);
            let digest = reader.blob().unwrap().to_vec();
            reader.end().unwrap();
            digest
        }

        /// A tpm-pin object as the protected seal will create it: the
        /// authValue as userAuth, `PROTECTED_ATTRIBUTES`, `policy` as its
        /// authPolicy. The test sends the authValue in the clear.
        fn create<T: Transport>(
            client: &mut Client<T>,
            policy: &PinPolicy,
            auth: &AuthValue,
        ) -> SealedObject {
            let (parent, _) = client.storage_primary(None).unwrap();
            let mut sensitive = Vec::new();
            put_blob(&mut sensitive, auth.expose()).unwrap();
            put_blob(&mut sensitive, &[0x5a; 32]).unwrap();
            let mut public = Vec::new();
            put16(&mut public, KEYED_HASH);
            put16(&mut public, td_tpm::SHA256);
            put32(&mut public, PROTECTED_ATTRIBUTES);
            put_blob(&mut public, &policy.digest()).unwrap();
            put16(&mut public, ALG_NULL);
            put_blob(&mut public, &[]).unwrap();
            let mut parameters = Vec::new();
            put_blob(&mut parameters, &sensitive).unwrap();
            put_blob(&mut parameters, &public).unwrap();
            put_blob(&mut parameters, &[]).unwrap();
            put32(&mut parameters, 0);
            let (_, out) = client
                .call(CREATE, &[parent], Some(PASSWORD), &parameters, false)
                .unwrap();
            client.flush(parent).unwrap();
            let mut reader = Reader(&out);
            let private = reader.blob().unwrap().to_vec();
            let public = reader.blob().unwrap().to_vec();
            SealedObject { public, private }
        }

        #[test]
        #[ignore = "needs TD_TEST_SWTPM, the pinned swtpm 0.10.1"]
        fn emulator_tpm_pin_policy_digest_and_chain_check() {
            let tpm = Emulator::start(&fresh_state("pin"));
            // The TPM computes the pinned literals itself.
            for (pcrs, values, expected) in [
                (
                    PinPcrs::Selector,
                    &[[0x44; 32], [0x49; 32]][..],
                    "81c76bdb2452e876666d16af17349d0e456703d57e6f2c73ca60d98f1c7d79c9",
                ),
                (
                    PinPcrs::SecureBoot,
                    &[[0x44; 32], [0x47; 32], [0x49; 32]][..],
                    "6aa4fd0a4446e73061ac0f7624582d8485a6915e9676435acbcd5b5b2abff7df",
                ),
            ] {
                let policy = PinPolicy::new(pcrs, values).unwrap();
                assert_eq!(
                    hex(&session_digest(&mut tpm.client(), &policy, true)),
                    expected
                );
            }

            tpm.client().extend_pcr(4, &[0x44; 32]).unwrap();
            tpm.client().extend_pcr(7, &[0x47; 32]).unwrap();
            tpm.client().extend_pcr(9, &[0x49; 32]).unwrap();
            // Before the cap, the TPM's own composite of PCRs 4, (7,) 9 and
            // the zero PCR 12 in a real session is the literal-zero one.
            for pcrs in [PinPcrs::Selector, PinPcrs::SecureBoot] {
                let policy = release_pin_policy(&mut tpm.client(), pcrs).unwrap();
                assert_eq!(
                    session_digest(&mut tpm.client(), &policy, false),
                    policy.digest()
                );
            }
            let (_, name) = tpm.client().storage_primary(None).unwrap();
            let name: [u8; PRIMARY_NAME_LEN] = name.try_into().unwrap();
            let policy = release_pin_policy(&mut tpm.client(), PinPcrs::Selector).unwrap();
            let auth = parsed(b"123456").auth_value(&[0x11; 32]);
            let sealed = create(&mut tpm.client(), &policy, &auth);
            assert_eq!(protected_auth_policy(&sealed.public), Ok(policy.digest()));
            chain_check(tpm.client(), PinPcrs::Selector, &name, &sealed).unwrap();
            assert_eq!(
                chain_check(tpm.client(), PinPcrs::SecureBoot, &name, &sealed),
                Err(ChainError::ChangedChain)
            );
            let mut forged = sealed.clone();
            let last = forged.private.len() - 1;
            forged.private[last] ^= 1;
            assert!(matches!(
                chain_check(tpm.client(), PinPcrs::Selector, &name, &forged),
                Err(ChainError::LoadRefused(Refusal { command: LOAD, .. }))
            ));
            tpm.client().extend_pcr(4, &[0x45; 32]).unwrap();
            assert_eq!(
                chain_check(tpm.client(), PinPcrs::Selector, &name, &sealed),
                Err(ChainError::ChangedChain)
            );

            // A fresh TPM state derives another storage primary.
            let fresh = Emulator::start(&fresh_state("pin-fresh"));
            assert_eq!(
                chain_check(fresh.client(), PinPcrs::Selector, &name, &sealed),
                Err(ChainError::OtherPrimary)
            );
        }
    }
}
