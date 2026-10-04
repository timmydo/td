//! td's device-bound disk protector policy over the shared td-tpm client:
//! the first-boot and observed PCR policies, the protector secret, sealing
//! and unsealing it, the PCR 12 release cap and the recovery key (DESIGN.md,
//! td-install/ENCRYPTION.md "Device-bound default").
#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use td_tpm::{Client, PcrPolicy, PcrSelection, SealedObject, Transport};

pub mod recovery;

/// The firmware's measurement of the selector EFI image.
pub const SELECTOR_IMAGE_PCR: u8 = 4;
/// The EFI stub's measurements of load options and the selector initramfs.
pub const SELECTOR_INITRD_PCR: u8 = 9;
/// Zero from platform reset until the selector's release cap.
pub const CAP_PCR: u8 = 12;
/// The device-bound protector's policy: PCRs 4, 9 and 12.
pub const DEVICE_BOUND_PCRS: &[u8] = &[SELECTOR_IMAGE_PCR, SELECTOR_INITRD_PCR, CAP_PCR];
/// The first-boot protector's policy: PCR 12 alone.
pub const FIRST_BOOT_PCRS: &[u8] = &[CAP_PCR];
/// Hashed with SHA-256 to give the release-cap event (`cap_event`).
pub const CAP_DOMAIN: &[u8] = b"td/disk-protector/release-cap/v1";
/// A protector secret: 32 bytes from the kernel CSPRNG.
pub const SECRET_LEN: usize = 32;
/// Blocks until the kernel CSPRNG is initialized, unlike `/dev/urandom`.
const SECRET_SOURCE: &str = "/dev/random";

/// Fill `out` from `source` in one exact read. Secrets and recovery keys
/// share it, so tests substitute a file for `/dev/random`.
fn read_random(source: &Path, out: &mut [u8], what: &str) -> Result<(), String> {
    File::open(source)
        .and_then(|mut file| file.read_exact(out))
        .map_err(|e| format!("read {what} from {}: {e}", source.display()))
}

/// The SHA-256 event the release cap extends into PCR 12.
pub fn cap_event() -> [u8; 32] {
    td_tpm::digest(CAP_DOMAIN)
}

fn selection(pcrs: &[u8]) -> Result<PcrSelection, String> {
    let mut mask = 0u16;
    for pcr in pcrs {
        mask |= 1u16
            .checked_shl(u32::from(*pcr))
            .ok_or("protector PCR outside 0..15")?;
    }
    PcrSelection::new(mask)
}

/// PCR 12 at its literal reset value. Nothing is read from the TPM.
pub fn first_boot_policy() -> Result<PcrPolicy, String> {
    Ok(PcrPolicy {
        selection: selection(FIRST_BOOT_PCRS)?,
        pcr_digest: td_tpm::pcr_digest(&[[0; 32]]),
    })
}

/// The observed PCR 4 and PCR 9 values and a literal-zero PCR 12. Only PCRs
/// 4 and 9 are read; an unmeasured (all-zero) value is refused.
pub fn observed_policy<T: Transport>(client: &mut Client<T>) -> Result<PcrPolicy, String> {
    let values = client.read_pcrs(selection(&[SELECTOR_IMAGE_PCR, SELECTOR_INITRD_PCR])?)?;
    let [image, initrd] = values.as_slice() else {
        return Err("TPM returned the wrong number of PCR values".into());
    };
    if *image == [0; 32] {
        return Err("PCR 4 is unmeasured: no selector image measurement".into());
    }
    if *initrd == [0; 32] {
        return Err("PCR 9 is unmeasured: no selector initramfs measurement".into());
    }
    Ok(PcrPolicy {
        selection: selection(DEVICE_BOUND_PCRS)?,
        pcr_digest: td_tpm::pcr_digest(&[*image, *initrd, [0; 32]]),
    })
}

/// A protector secret. It lives in one heap allocation that is zeroed on
/// drop, and it is deliberately neither `Debug`, `Display` nor `Clone`.
pub struct Secret(Box<[u8; SECRET_LEN]>);
impl Drop for Secret {
    fn drop(&mut self) {
        td_tpm::zero(self.0.as_mut_slice());
    }
}
impl Secret {
    /// 32 bytes from `/dev/random`, which blocks until the kernel CSPRNG is
    /// initialized. The selector generates protectors in early boot, and a
    /// weak secret behind its minimal-cost PBKDF2 keyslot would bypass the
    /// TPM.
    pub fn generate() -> Result<Self, String> {
        Self::read_from(Path::new(SECRET_SOURCE))
    }

    fn read_from(source: &Path) -> Result<Self, String> {
        let mut secret = Self(Box::new([0; SECRET_LEN]));
        read_random(source, secret.0.as_mut_slice(), "protector secret")?;
        Ok(secret)
    }

    /// Takes an unsealed payload, zeroing it whether or not it is accepted.
    fn from_payload(mut payload: Vec<u8>) -> Result<Self, String> {
        let mut secret = Self(Box::new([0; SECRET_LEN]));
        let accepted = payload.len() == SECRET_LEN;
        if accepted {
            secret.0.copy_from_slice(&payload);
        }
        td_tpm::zero(&mut payload);
        if !accepted {
            return Err(format!(
                "unsealed protector holds {} bytes, not {SECRET_LEN}",
                payload.len()
            ));
        }
        Ok(secret)
    }

    /// The secret bytes, for a keyslot operation that must not copy them
    /// into argv, the environment, a log or a persistent file.
    pub fn expose(&self) -> &[u8; SECRET_LEN] {
        &self.0
    }
}

/// Seal `secret` under `policy` beneath the unpersonalized owner storage
/// primary. The sealed object carries no secret in the clear.
pub fn seal<T: Transport>(
    client: Client<T>,
    policy: &PcrPolicy,
    secret: &Secret,
) -> Result<SealedObject, String> {
    let mut payload = *secret.expose();
    let sealed = client.seal_object(policy, None, &mut payload);
    // seal_object zeroes the payload on every path; this is belt and braces.
    td_tpm::zero(&mut payload);
    sealed
}

/// Unseal a protector under `policy`, refusing any payload but 32 bytes.
pub fn unseal<T: Transport>(
    client: Client<T>,
    policy: &PcrPolicy,
    sealed: &SealedObject,
) -> Result<Secret, String> {
    Secret::from_payload(client.unseal_object(policy, None, &sealed.public, &sealed.private)?)
}

/// Why the release cap did not complete (DESIGN.md "Release cap").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapError {
    /// PCR 12 was already non-zero, so nothing was extended: no TPM release
    /// is possible this boot.
    AlreadyClosed,
    /// A PCR 12 read or the extension failed in transport or was refused:
    /// whether PCR 12 moved is unknown.
    Uncertain(String),
    /// The readback was not `SHA256(zero32 || cap_event())`.
    Mismatch,
}
impl fmt::Display for CapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyClosed => {
                f.write_str("PCR 12 release cap: PCR 12 already extended; no TPM release this boot")
            }
            Self::Uncertain(error) => write!(
                f,
                "PCR 12 release cap uncertain; platform reset required: {error}"
            ),
            Self::Mismatch => {
                f.write_str("PCR 12 release cap readback mismatch; platform reset required")
            }
        }
    }
}
impl std::error::Error for CapError {}

/// Extend PCR 12 with the release-cap event once and require the exact
/// readback `SHA256(zero32 || cap_event())`. PCR 12 must read zero first;
/// a non-zero prior is `AlreadyClosed` and is not extended again. Never
/// retried. The caller's handling of each outcome is DESIGN.md's.
pub fn cap<T: Transport>(client: &mut Client<T>) -> Result<(), CapError> {
    let prior = client.read_pcr(CAP_PCR).map_err(CapError::Uncertain)?;
    if prior != [0; 32] {
        return Err(CapError::AlreadyClosed);
    }
    let event = cap_event();
    client
        .extend_pcr(CAP_PCR, &event)
        .map_err(CapError::Uncertain)?;
    let expected = td_tpm::digest(&[[0; 32], event].concat());
    let capped = client.read_pcr(CAP_PCR).map_err(CapError::Uncertain)?;
    if capped != expected {
        return Err(CapError::Mismatch);
    }
    Ok(())
}

/// The installer's check of a sealed first-boot protector: its authPolicy
/// is the PCR-12-at-zero policy the TPM itself computes in a trial session,
/// and the TPM loads the pair under the storage primary, so the private
/// area verifies. Nothing is unsealed: PCR 12 is already capped.
pub fn verify_first_boot_object<T: Transport>(
    client: &mut Client<T>,
    sealed: &SealedObject,
) -> Result<(), String> {
    let policy = first_boot_policy()?;
    let trial = client.policy_session(&policy, true)?;
    client.flush(trial)?;
    td_tpm::validate_sealed_public(&sealed.public, &policy.digest())?;
    client.load_and_flush(None, &sealed.public, &sealed.private)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use td_tpm::{
        put16, put32, put_blob, Reader, ALG_NULL, CREATE, CREATE_PRIMARY, FLUSH_CONTEXT, LOAD,
        NO_SESSIONS, NULL, OWNER, PASSWORD, PCR_EXTEND, PCR_READ, POLICY_COMMAND_CODE,
        POLICY_GET_DIGEST, POLICY_PCR, SESSIONS, SHA256, START_AUTH_SESSION, UNSEAL,
    };

    const POLICY_SESSION: u8 = 0x01;
    const TRIAL_SESSION: u8 = 0x03;
    /// TPM_RC_VALUE on parameter 1, TPM_RC_POLICY_FAIL on session 1, and
    /// TPM_RC_LOCALITY for a refused extension.
    const RC_VALUE: u32 = 0x1c4;
    const RC_POLICY_FAIL: u32 = 0x99d;
    const RC_LOCALITY: u32 = 0x907;
    /// TPM_RC_INTEGRITY on parameter 1: a private area that fails Load.
    const RC_INTEGRITY: u32 = 0x19f;

    /// An unseal's refusal; `Secret` is not `Debug`, so no `unwrap_err`.
    fn refusal<T: Transport>(
        client: Client<T>,
        policy: &PcrPolicy,
        sealed: &SealedObject,
    ) -> String {
        match unseal(client, policy, sealed) {
            Ok(_) => panic!("the protector was released"),
            Err(error) => error,
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn response(tag: u16, rc: u32, body: &[u8]) -> Vec<u8> {
        let mut out = tag.to_be_bytes().to_vec();
        put32(&mut out, 10 + body.len() as u32);
        put32(&mut out, rc);
        out.extend_from_slice(body);
        out
    }

    /// A scripted TPM with PCR state, ported from td-tpm's: a real policy
    /// session's PolicyPCR refuses a composite other than its PCRs, a trial
    /// takes the caller's, and Unseal refuses a session whose digest is not
    /// the loaded object's authPolicy.
    struct Tpm {
        codes: Vec<u32>,
        reads: Vec<u16>,
        pcrs: [[u8; 32]; 16],
        next: u32,
        sessions: Vec<(u32, bool, [u8; 32])>,
        objects: Vec<(u32, Vec<u8>, Vec<u8>)>,
        /// Extend with another event than the one sent.
        skew_extend: bool,
        refuse_extend: bool,
        /// Run the command but lose its reply: the nth exchange, from one.
        lose: Option<usize>,
    }

    #[derive(Clone)]
    struct Scripted(Rc<RefCell<Tpm>>);
    impl Scripted {
        /// PCRs 4 and 9 measured, every other PCR at reset.
        fn new() -> Self {
            let mut pcrs = [[0; 32]; 16];
            pcrs[4] = [0x44; 32];
            pcrs[9] = [0x49; 32];
            Self(Rc::new(RefCell::new(Tpm {
                codes: Vec::new(),
                reads: Vec::new(),
                pcrs,
                next: 0,
                sessions: Vec::new(),
                objects: Vec::new(),
                skew_extend: false,
                refuse_extend: false,
                lose: None,
            })))
        }

        fn client(&self) -> Client<Self> {
            Client::new(self.clone())
        }

        fn codes(&self) -> Vec<u32> {
            std::mem::take(&mut self.0.borrow_mut().codes)
        }

        fn reads(&self) -> Vec<u16> {
            std::mem::take(&mut self.0.borrow_mut().reads)
        }

        fn pcr(&self, index: usize) -> [u8; 32] {
            self.0.borrow().pcrs[index]
        }

        fn idle(&self) -> bool {
            let tpm = self.0.borrow();
            tpm.sessions.is_empty() && tpm.objects.is_empty()
        }
    }
    impl Transport for Scripted {
        fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
            let mut tpm = self.0.borrow_mut();
            let reply = tpm.reply(command);
            if tpm.lose == Some(tpm.codes.len()) {
                return Err("lost reply".into());
            }
            Ok(reply)
        }
    }

    fn scripted_selection(input: &mut Reader<'_>) -> (Vec<u8>, u16) {
        assert_eq!(input.u32().unwrap(), 1);
        assert_eq!(input.u16().unwrap(), SHA256);
        assert_eq!(input.u8().unwrap(), 3);
        let mask = input.take(3).unwrap();
        assert_eq!(mask[2], 0);
        let mask = u16::from_le_bytes([mask[0], mask[1]]);
        (PcrSelection::new(mask).unwrap().marshal(), mask)
    }

    fn scripted_creation(out: &mut Vec<u8>) {
        put_blob(out, &[]).unwrap();
        put_blob(out, &[0x0c; 32]).unwrap();
        put16(out, 0x8021);
        put32(out, OWNER);
        put_blob(out, &[0x0d; 32]).unwrap();
    }

    fn scripted_name(public: &[u8]) -> Vec<u8> {
        let mut name = SHA256.to_be_bytes().to_vec();
        name.extend_from_slice(&td_tpm::digest(public));
        name
    }

    impl Tpm {
        fn composite(&self, mask: u16) -> [u8; 32] {
            let values: Vec<[u8; 32]> = (0..16)
                .filter(|index| mask & (1 << index) != 0)
                .map(|index| self.pcrs[index])
                .collect();
            td_tpm::pcr_digest(&values)
        }

        fn session(&mut self, handle: Option<u32>) -> &mut (u32, bool, [u8; 32]) {
            self.sessions
                .iter_mut()
                .find(|(value, ..)| Some(*value) == handle)
                .expect("unknown policy session")
        }

        fn extend_policy(&mut self, handle: Option<u32>, code: u32, tail: &[&[u8]]) {
            let state = &mut self.session(handle).2;
            let mut bytes = state.to_vec();
            put32(&mut bytes, code);
            for part in tail {
                bytes.extend_from_slice(part);
            }
            *state = td_tpm::digest(&bytes);
        }

        fn reply(&mut self, command: &[u8]) -> Vec<u8> {
            let tag = u16::from_be_bytes([command[0], command[1]]);
            let size = u32::from_be_bytes(command[2..6].try_into().unwrap());
            assert_eq!(size as usize, command.len());
            let code = u32::from_be_bytes(command[6..10].try_into().unwrap());
            self.codes.push(code);
            self.next += 1;
            let mut input = Reader(&command[10..]);
            let handle_count = match code {
                PCR_READ => 0,
                START_AUTH_SESSION => 2,
                _ => 1,
            };
            let handles: Vec<u32> = (0..handle_count).map(|_| input.u32().unwrap()).collect();
            let handle = handles.first().copied();
            let mut session = None;
            if tag == SESSIONS {
                let size = input.u32().unwrap() as usize;
                let mut area = Reader(input.take(size).unwrap());
                let auth = area.u32().unwrap();
                let nonce = area.blob().unwrap();
                assert_eq!(area.u8().unwrap(), 0);
                assert!(area.blob().unwrap().is_empty());
                area.end().unwrap();
                if auth == PASSWORD {
                    assert!(nonce.is_empty());
                } else {
                    assert_eq!(nonce.len(), 32);
                    session = Some(auth);
                }
            } else {
                assert_eq!(tag, NO_SESSIONS);
            }
            let mut out = Vec::new();
            let mut out_handle = None;
            match code {
                PCR_READ => {
                    let (selection, mask) = scripted_selection(&mut input);
                    input.end().unwrap();
                    self.reads.push(mask);
                    put32(&mut out, 0x55);
                    out.extend_from_slice(&selection);
                    put32(&mut out, mask.count_ones());
                    for index in (0..16).filter(|index| mask & (1 << index) != 0) {
                        put_blob(&mut out, &self.pcrs[index]).unwrap();
                    }
                }
                PCR_EXTEND => {
                    assert_eq!(tag, SESSIONS);
                    assert_eq!(session, None);
                    assert_eq!(input.u32().unwrap(), 1);
                    assert_eq!(input.u16().unwrap(), SHA256);
                    let mut event = input.take(32).unwrap().to_vec();
                    input.end().unwrap();
                    if self.refuse_extend {
                        return response(NO_SESSIONS, RC_LOCALITY, &[]);
                    }
                    if self.skew_extend {
                        event[0] ^= 1;
                    }
                    let pcr = &mut self.pcrs[handle.unwrap() as usize];
                    *pcr = td_tpm::digest(&[&pcr[..], &event].concat());
                }
                START_AUTH_SESSION => {
                    assert_eq!(handles, [NULL, NULL]);
                    assert_eq!(input.blob().unwrap().len(), 32);
                    assert!(input.blob().unwrap().is_empty());
                    let kind = input.u8().unwrap();
                    assert!(matches!(kind, POLICY_SESSION | TRIAL_SESSION));
                    assert_eq!(input.u16().unwrap(), ALG_NULL);
                    assert_eq!(input.u16().unwrap(), SHA256);
                    input.end().unwrap();
                    let handle = 0x0300_0000 + self.next;
                    self.sessions.push((handle, kind == TRIAL_SESSION, [0; 32]));
                    out_handle = Some(handle);
                    put_blob(&mut out, &[0x0e; 32]).unwrap();
                }
                POLICY_PCR => {
                    let composite = input.blob().unwrap();
                    let (selection, mask) = scripted_selection(&mut input);
                    input.end().unwrap();
                    let trial = self.session(handle).1;
                    if !trial && composite != self.composite(mask) {
                        return response(NO_SESSIONS, RC_VALUE, &[]);
                    }
                    self.extend_policy(handle, POLICY_PCR, &[&selection, composite]);
                }
                POLICY_COMMAND_CODE => {
                    let command_code = input.take(4).unwrap();
                    input.end().unwrap();
                    self.extend_policy(handle, POLICY_COMMAND_CODE, &[command_code]);
                }
                POLICY_GET_DIGEST => {
                    input.end().unwrap();
                    let state = self.session(handle).2;
                    put_blob(&mut out, &state).unwrap();
                }
                CREATE_PRIMARY => {
                    assert_eq!(handle, Some(OWNER));
                    assert_eq!(input.blob().unwrap(), [0; 4]);
                    let template = input.blob().unwrap();
                    assert!(input.blob().unwrap().is_empty());
                    assert_eq!(input.u32().unwrap(), 0);
                    input.end().unwrap();
                    // The protector is never personalized: unique.x is empty.
                    assert_eq!(&template[22..], [0, 0, 0, 0]);
                    let mut public = template[..22].to_vec();
                    put_blob(&mut public, &td_tpm::digest(template)).unwrap();
                    put_blob(&mut public, &[0x0f; 32]).unwrap();
                    out_handle = Some(0x8000_0000 + self.next);
                    put_blob(&mut out, &public).unwrap();
                    scripted_creation(&mut out);
                    put_blob(&mut out, &scripted_name(&public)).unwrap();
                }
                CREATE => {
                    let mut sensitive = Reader(input.blob().unwrap());
                    assert!(sensitive.blob().unwrap().is_empty());
                    let data = sensitive.blob().unwrap();
                    sensitive.end().unwrap();
                    let template = input.blob().unwrap();
                    assert!(input.blob().unwrap().is_empty());
                    assert_eq!(input.u32().unwrap(), 0);
                    input.end().unwrap();
                    let mut public = template[..template.len() - 2].to_vec();
                    put_blob(&mut public, &td_tpm::digest(data)).unwrap();
                    let mut private = b"scripted:".to_vec();
                    private.extend_from_slice(data);
                    put_blob(&mut out, &private).unwrap();
                    put_blob(&mut out, &public).unwrap();
                    scripted_creation(&mut out);
                }
                LOAD => {
                    let private = input.blob().unwrap();
                    let public = input.blob().unwrap();
                    input.end().unwrap();
                    let mut fields = Reader(public);
                    fields.take(8).unwrap();
                    let policy = fields.blob().unwrap().to_vec();
                    let Some(data) = private.strip_prefix(b"scripted:") else {
                        return response(NO_SESSIONS, RC_INTEGRITY, &[]);
                    };
                    let data = data.to_vec();
                    let handle = 0x8000_0000 + self.next;
                    self.objects.push((handle, policy, data));
                    out_handle = Some(handle);
                    put_blob(&mut out, &scripted_name(public)).unwrap();
                }
                UNSEAL => {
                    input.end().unwrap();
                    let state = *self.session(session);
                    let (_, policy, data) = self
                        .objects
                        .iter()
                        .find(|(value, ..)| Some(*value) == handle)
                        .unwrap();
                    if state.1 || policy[..] != state.2 {
                        return response(NO_SESSIONS, RC_POLICY_FAIL, &[]);
                    }
                    put_blob(&mut out, data).unwrap();
                    self.sessions.retain(|(value, ..)| Some(*value) != session);
                }
                FLUSH_CONTEXT => {
                    input.end().unwrap();
                    self.sessions.retain(|(value, ..)| Some(*value) != handle);
                    self.objects.retain(|(value, ..)| Some(*value) != handle);
                }
                _ => panic!("unscripted TPM command {code:#x}"),
            }
            let mut body = Vec::new();
            if let Some(handle) = out_handle {
                put32(&mut body, handle);
            }
            if tag == SESSIONS {
                put32(&mut body, out.len() as u32);
                body.extend_from_slice(&out);
                if session.is_some() {
                    put_blob(&mut body, &[0x0b; 32]).unwrap();
                    body.push(0);
                } else {
                    put_blob(&mut body, &[]).unwrap();
                    body.push(1);
                }
                put_blob(&mut body, &[]).unwrap();
            } else {
                body.extend_from_slice(&out);
            }
            response(tag, 0, &body)
        }
    }

    /// The cap event and the first-boot policy digest are persisted
    /// contracts: installed selectors cap with the one, and sealed
    /// first-boot protectors carry the other as their authPolicy.
    #[test]
    fn cap_event_and_policy_digests_keep_their_literals() {
        assert_eq!(CAP_DOMAIN, b"td/disk-protector/release-cap/v1");
        assert_eq!(
            hex(&cap_event()),
            "dbeb06a2ce317c919c4a6559db4efa374a69a6c033af58fdb0ec327fcf9a3aee"
        );
        let first = first_boot_policy().unwrap();
        assert_eq!(first.selection.mask(), 1 << 12);
        assert_eq!(first.pcr_digest, td_tpm::digest(&[0; 32]));
        // PolicyPCR(PCR 12 = zero) then PolicyCommandCode(Unseal).
        assert_eq!(
            hex(&first.digest()),
            "b4df7c41e616f0b1dfccebd6f2ee275d0d5577854801dec255792b348d0eb4cd"
        );
        let tpm = Scripted::new();
        let observed = observed_policy(&mut tpm.client()).unwrap();
        assert_eq!(observed.selection.mask(), 1 << 4 | 1 << 9 | 1 << 12);
        // PolicyPCR(PCRs 4, 9, 12 = [0x44; 32], [0x49; 32], zero) then
        // PolicyCommandCode(Unseal).
        assert_eq!(
            hex(&observed.digest()),
            "5a056b792ba49d634282c277982d00a8ba7420926eeb9771495f9fc46bd17678"
        );
        assert_eq!(DEVICE_BOUND_PCRS, [4, 9, 12]);
        assert_eq!(FIRST_BOOT_PCRS, [12]);
    }

    #[test]
    fn first_boot_protector_seals_without_reading_and_unseals_until_the_cap() {
        let tpm = Scripted::new();
        let policy = first_boot_policy().unwrap();
        let secret = Secret::generate().unwrap();
        let sealed = seal(tpm.client(), &policy, &secret).unwrap();
        assert!(
            tpm.reads().is_empty(),
            "the first-boot policy reads nothing"
        );
        tpm.codes();
        verify_first_boot_object(&mut tpm.client(), &sealed).unwrap();
        assert_eq!(
            tpm.codes(),
            [
                START_AUTH_SESSION,
                POLICY_PCR,
                POLICY_COMMAND_CODE,
                POLICY_GET_DIGEST,
                FLUSH_CONTEXT,
                CREATE_PRIMARY,
                LOAD,
                FLUSH_CONTEXT,
                FLUSH_CONTEXT,
            ]
        );
        assert!(tpm.idle());
        // A private area the TPM will not load is refused, and nothing stays
        // loaded.
        let mut forged = sealed.clone();
        forged.private[0] ^= 1;
        let refused = verify_first_boot_object(&mut tpm.client(), &forged).unwrap_err();
        assert_eq!(refused, "TPM command 0x157 refused: 0x19f");
        assert!(tpm.idle());
        let unsealed = unseal(tpm.client(), &policy, &sealed).unwrap();
        assert_eq!(unsealed.expose(), secret.expose());
        assert!(tpm.idle());
        tpm.codes();

        cap(&mut tpm.client()).unwrap();
        assert_eq!(tpm.codes(), [PCR_READ, PCR_EXTEND, PCR_READ]);
        let refused = refusal(tpm.client(), &policy, &sealed);
        assert!(refused.contains("0x17f refused"), "{refused}");
        assert!(!tpm.codes().contains(&UNSEAL));
        assert!(tpm.idle());
    }

    #[test]
    fn observed_protector_reads_only_pcrs_4_and_9_and_is_closed_by_the_cap() {
        let tpm = Scripted::new();
        let policy = observed_policy(&mut tpm.client()).unwrap();
        assert_eq!(tpm.reads(), [1 << 4 | 1 << 9]);
        let secret = Secret::generate().unwrap();
        let sealed = seal(tpm.client(), &policy, &secret).unwrap();
        td_tpm::validate_sealed_public(&sealed.public, &policy.digest()).unwrap();
        // The device-bound object is not the first-boot policy.
        assert!(verify_first_boot_object(&mut tpm.client(), &sealed).is_err());
        let unsealed = unseal(tpm.client(), &policy, &sealed).unwrap();
        assert_eq!(unsealed.expose(), secret.expose());
        // PCR 12 entered the policy as a literal and was never read.
        assert!(tpm.reads().iter().all(|mask| mask & 1 << 12 == 0));
        tpm.codes();

        cap(&mut tpm.client()).unwrap();
        assert!(refusal(tpm.client(), &policy, &sealed).contains("0x17f refused"));
        assert!(!tpm.codes().contains(&UNSEAL));

        // A changed selector image also refuses at PolicyPCR.
        let tpm = Scripted::new();
        let sealed = seal(tpm.client(), &policy, &secret).unwrap();
        tpm.0.borrow_mut().pcrs[4] = [0x45; 32];
        assert!(refusal(tpm.client(), &policy, &sealed).contains("0x17f refused"));
        assert!(!tpm.codes().contains(&UNSEAL));
    }

    #[test]
    fn an_unmeasured_selector_pcr_refuses_the_observed_policy() {
        for index in [4, 9] {
            let tpm = Scripted::new();
            tpm.0.borrow_mut().pcrs[index] = [0; 32];
            let refused = observed_policy(&mut tpm.client()).unwrap_err();
            assert!(refused.contains(&format!("PCR {index} is unmeasured")));
            assert_eq!(tpm.codes(), [PCR_READ]);
        }
    }

    #[test]
    fn the_cap_types_a_closed_pcr_an_uncertain_extension_and_a_wrong_readback() {
        let event = cap_event();
        let tpm = Scripted::new();
        cap(&mut tpm.client()).unwrap();
        assert_eq!(tpm.pcr(12), td_tpm::digest(&[[0; 32], event].concat()));
        tpm.codes();
        // Already capped: reported, not extended again.
        let closed = cap(&mut tpm.client()).unwrap_err();
        assert_eq!(closed, CapError::AlreadyClosed);
        assert_eq!(tpm.codes(), [PCR_READ]);
        assert_eq!(
            closed.to_string(),
            "PCR 12 release cap: PCR 12 already extended; no TPM release this boot"
        );

        let tpm = Scripted::new();
        tpm.0.borrow_mut().refuse_extend = true;
        let refused = cap(&mut tpm.client()).unwrap_err();
        assert_eq!(
            refused,
            CapError::Uncertain("TPM command 0x182 refused: 0x907".into())
        );
        assert_eq!(tpm.codes(), [PCR_READ, PCR_EXTEND]);
        assert_eq!(
            refused.to_string(),
            "PCR 12 release cap uncertain; platform reset required: \
             TPM command 0x182 refused: 0x907"
        );

        // A lost reply at each exchange: the prior read, the extension that
        // reached the TPM, and the readback. None is retried.
        for (lose, codes) in [
            (1, &[PCR_READ][..]),
            (2, &[PCR_READ, PCR_EXTEND][..]),
            (3, &[PCR_READ, PCR_EXTEND, PCR_READ][..]),
        ] {
            let tpm = Scripted::new();
            tpm.0.borrow_mut().lose = Some(lose);
            assert_eq!(
                cap(&mut tpm.client()),
                Err(CapError::Uncertain("lost reply".into()))
            );
            assert_eq!(tpm.codes(), codes);
        }

        let tpm = Scripted::new();
        tpm.0.borrow_mut().skew_extend = true;
        let mismatch = cap(&mut tpm.client()).unwrap_err();
        assert_eq!(mismatch, CapError::Mismatch);
        assert_eq!(tpm.codes(), [PCR_READ, PCR_EXTEND, PCR_READ]);
        assert_eq!(
            mismatch.to_string(),
            "PCR 12 release cap readback mismatch; platform reset required"
        );
    }

    #[test]
    fn an_unsealed_payload_must_be_exactly_32_bytes() {
        let policy = first_boot_policy().unwrap();
        for size in [1, 31, 33, td_tpm::MAX_SEALED_PAYLOAD] {
            let tpm = Scripted::new();
            let mut payload = vec![0x5a; size];
            let sealed = tpm
                .client()
                .seal_object(&policy, None, &mut payload)
                .unwrap();
            let refused = refusal(tpm.client(), &policy, &sealed);
            assert!(
                refused.contains(&format!("holds {size} bytes")),
                "{refused}"
            );
        }
        assert_eq!(
            *Secret::from_payload(vec![1; 32]).unwrap().expose(),
            [1; 32]
        );
    }

    #[test]
    fn secrets_come_from_dev_random_in_one_exact_read() {
        assert_eq!(SECRET_SOURCE, "/dev/random");
        let dir = std::env::temp_dir().join(format!(
            "td-protector-source-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let source = dir.join("random");
        let bytes: Vec<u8> = (0..40).collect();
        std::fs::write(&source, &bytes).unwrap();
        assert_eq!(
            Secret::read_from(&source).unwrap().expose()[..],
            bytes[..SECRET_LEN]
        );
        std::fs::write(&source, &bytes[..SECRET_LEN - 1]).unwrap();
        let short = match Secret::read_from(&source) {
            Ok(_) => panic!("a short source was accepted"),
            Err(error) => error,
        };
        assert!(
            short.starts_with(&format!(
                "read protector secret from {}: ",
                source.display()
            )),
            "{short}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(Secret::read_from(&source).is_err());
    }

    #[test]
    fn generated_secrets_differ() {
        let first = Secret::generate().unwrap();
        let second = Secret::generate().unwrap();
        assert_ne!(first.expose(), second.expose());
        assert_ne!(*first.expose(), [0; SECRET_LEN]);
    }

    /// The swtpm oracle, run as td-secret/DESIGN.md "TPM validation" runs
    /// td-secret's: `TD_TEST_SWTPM=/absolute/path/to/swtpm cargo test
    /// --frozen --manifest-path td-protector/Cargo.toml emulator_ --
    /// --ignored`.
    mod emulator {
        use super::*;
        use std::io::Write;
        use std::os::unix::net::UnixStream;
        use std::path::{Path, PathBuf};
        use std::process::{Child, Command, Stdio};
        use std::time::{Duration, Instant};

        struct Socket(UnixStream);
        impl Transport for Socket {
            fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
                self.0.write_all(command).map_err(|e| e.to_string())?;
                let mut header = [0; 10];
                self.0.read_exact(&mut header).map_err(|e| e.to_string())?;
                let size = u32::from_be_bytes(header[2..6].try_into().unwrap()) as usize;
                if !(10..=td_tpm::MAX_PACKET).contains(&size) {
                    return Err("invalid test TPM packet".into());
                }
                let mut reply = header.to_vec();
                reply.resize(size, 0);
                self.0
                    .read_exact(&mut reply[10..])
                    .map_err(|e| e.to_string())?;
                Ok(reply)
            }
        }

        struct Emulator {
            child: Child,
            state: PathBuf,
            socket: PathBuf,
        }
        impl Emulator {
            fn start(state: &Path) -> Self {
                assert!(!state.exists());
                std::fs::create_dir_all(state).unwrap();
                let socket = state.join("socket");
                let executable = std::env::var_os("TD_TEST_SWTPM")
                    .expect("set TD_TEST_SWTPM to pinned swtpm 0.10.1");
                let version = Command::new(&executable).arg("--version").output().unwrap();
                assert!(version.status.success());
                assert!(
                    String::from_utf8_lossy(&version.stdout)
                        .starts_with("TPM emulator version 0.10.1,"),
                    "oracle requires swtpm 0.10.1"
                );
                let child = Command::new(executable)
                    .args(["socket", "--tpm2", "--tpmstate"])
                    .arg(format!("dir={},mode=0600", state.display()))
                    .arg("--server")
                    .arg(format!("type=unixio,path={}", socket.display()))
                    .args(["--flags", "not-need-init,startup-clear"])
                    .stdout(Stdio::null())
                    .spawn()
                    .unwrap();
                let mut result = Self {
                    child,
                    state: state.to_path_buf(),
                    socket,
                };
                let deadline = Instant::now() + Duration::from_secs(10);
                while !result.socket.exists() {
                    assert!(result.child.try_wait().unwrap().is_none(), "swtpm exited");
                    assert!(Instant::now() < deadline, "swtpm startup timeout");
                    std::thread::sleep(Duration::from_millis(10));
                }
                result
            }

            fn client(&self) -> Client<Socket> {
                let socket = UnixStream::connect(&self.socket).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                Client::new(Socket(socket))
            }
        }
        impl Drop for Emulator {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
                let _ = std::fs::remove_dir_all(&self.state);
            }
        }

        #[test]
        #[ignore = "needs TD_TEST_SWTPM, the pinned swtpm 0.10.1"]
        fn emulator_protectors_release_until_the_cap() {
            let state = std::env::temp_dir().join(format!(
                "td-protector-oracle-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let tpm = Emulator::start(&state);
            // The emulator has no firmware: the observed policy is refused
            // until fixture selector measurements reach PCRs 4 and 9.
            assert!(observed_policy(&mut tpm.client())
                .unwrap_err()
                .contains("PCR 4 is unmeasured"));
            tpm.client().extend_pcr(4, &[0x44; 32]).unwrap();
            tpm.client().extend_pcr(9, &[0x49; 32]).unwrap();

            let first = first_boot_policy().unwrap();
            let secret = Secret::generate().unwrap();
            let first_sealed = seal(tpm.client(), &first, &secret).unwrap();
            verify_first_boot_object(&mut tpm.client(), &first_sealed).unwrap();
            let unsealed = unseal(tpm.client(), &first, &first_sealed).unwrap();
            assert_eq!(unsealed.expose(), secret.expose());
            let observed = observed_policy(&mut tpm.client()).unwrap();
            let bound = Secret::generate().unwrap();
            let bound_sealed = seal(tpm.client(), &observed, &bound).unwrap();
            assert!(verify_first_boot_object(&mut tpm.client(), &bound_sealed).is_err());
            let unsealed = unseal(tpm.client(), &observed, &bound_sealed).unwrap();
            assert_eq!(unsealed.expose(), bound.expose());

            cap(&mut tpm.client()).unwrap();
            assert!(unseal(tpm.client(), &first, &first_sealed).is_err());
            assert!(unseal(tpm.client(), &observed, &bound_sealed).is_err());
            assert_eq!(cap(&mut tpm.client()), Err(CapError::AlreadyClosed));
        }
    }
}
