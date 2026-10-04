//! td's TPM 2.0 client: bounded command transport, password and policy
//! sessions, the owner storage primary, PCR policy sealing, and PCR
//! extension. Envelope formats and refusal policy belong to callers
//! (DESIGN.md).
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

#[path = "../../engine/src/sha256.rs"]
#[allow(
    dead_code,
    reason = "the shared hash also supports build artifact files"
)]
mod sha256;

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};

pub const NO_SESSIONS: u16 = 0x8001;
pub const SESSIONS: u16 = 0x8002;
pub const OWNER: u32 = 0x4000_0001;
pub const NULL: u32 = 0x4000_0007;
pub const PASSWORD: u32 = 0x4000_0009;
pub const SHA256: u16 = 0x000b;
pub const ALG_NULL: u16 = 0x0010;
pub const CREATE_PRIMARY: u32 = 0x131;
pub const CREATE: u32 = 0x153;
pub const LOAD: u32 = 0x157;
pub const UNSEAL: u32 = 0x15e;
pub const FLUSH_CONTEXT: u32 = 0x165;
pub const POLICY_COMMAND_CODE: u32 = 0x16c;
pub const START_AUTH_SESSION: u32 = 0x176;
pub const PCR_READ: u32 = 0x17e;
pub const POLICY_PCR: u32 = 0x17f;
pub const PCR_EXTEND: u32 = 0x182;
pub const POLICY_GET_DIGEST: u32 = 0x189;
/// fixedTPM | fixedParent | adminWithPolicy | noDA, with userWithAuth
/// clear: the sealed object's policy is its only release path.
pub const SEALED_ATTRIBUTES: u32 = 0x492;
pub const MAX_PACKET: usize = 4096;
/// TPM2B_SENSITIVE_DATA's minimum guaranteed capacity (MAX_SYM_DATA).
pub const MAX_SEALED_PAYLOAD: usize = 128;
const O_NOFOLLOW: i32 = 0o400000;
const TRIAL_SESSION: u8 = 0x03;
const POLICY_SESSION: u8 = 0x01;

pub fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut hash = sha256::Sha256::new();
    hash.update(bytes);
    hash.finalize()
}

pub trait Transport {
    fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String>;
}

/// The kernel resource manager, which also owns the connection lifetime.
pub struct Device(File);
impl Device {
    pub fn open() -> Result<Self, String> {
        Self::open_io().map_err(|e| e.to_string())
    }

    /// As `open`, keeping the OS error kind for a caller that reports it.
    pub fn open_io() -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NOFOLLOW)
            .open("/dev/tpmrm0")
            .map_err(|e| io::Error::new(e.kind(), format!("open TPM resource manager: {e}")))?;
        let metadata = file
            .metadata()
            .map_err(|e| io::Error::new(e.kind(), format!("stat TPM resource manager: {e}")))?;
        if !metadata.file_type().is_char_device() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "TPM resource manager is not a character device",
            ));
        }
        Ok(Self(file))
    }
}
impl Transport for Device {
    fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
        // TPM device writes are complete commands, not a byte stream.
        let written = self
            .0
            .write(command)
            .map_err(|e| format!("write TPM: {e}"))?;
        if written != command.len() {
            return Err("short TPM command write".into());
        }
        let mut reply = vec![0; MAX_PACKET];
        let size = self
            .0
            .read(&mut reply)
            .map_err(|e| format!("read TPM: {e}"))?;
        reply.truncate(size);
        Ok(reply)
    }
}

/// A SHA-256 PCR selection: one to eight of the static PCRs 0 through 15.
/// This bounds the selection the client marshals and the PCR_Read reply
/// it accepts; which PCRs a policy selects is the consumer's policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcrSelection(u16);
impl PcrSelection {
    pub fn new(mask: u16) -> Result<Self, String> {
        if mask == 0 {
            return Err("empty PCR selection".into());
        }
        if mask.count_ones() > 8 {
            return Err("select at most eight PCRs".into());
        }
        Ok(Self(mask))
    }

    pub fn mask(self) -> u16 {
        self.0
    }

    pub fn count(self) -> u32 {
        self.0.count_ones()
    }

    /// TPML_PCR_SELECTION: one SHA-256 bank with a three-byte bitmap.
    pub fn marshal(self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(10);
        put32(&mut bytes, 1);
        put16(&mut bytes, SHA256);
        bytes.push(3);
        bytes.extend_from_slice(&self.0.to_le_bytes());
        bytes.push(0);
        bytes
    }
}

/// The SHA-256 composite TPM2_PolicyPCR compares: the selected values,
/// concatenated in ascending PCR order.
pub fn pcr_digest(values: &[[u8; 32]]) -> [u8; 32] {
    digest(&values.concat())
}

/// PolicyPCR(selection, pcr_digest) then PolicyCommandCode(Unseal), both
/// extended from the zero digest.
pub fn policy_digest(selection: PcrSelection, pcr_digest: &[u8; 32]) -> [u8; 32] {
    let mut bytes = vec![0; 32];
    put32(&mut bytes, POLICY_PCR);
    bytes.extend_from_slice(&selection.marshal());
    bytes.extend_from_slice(pcr_digest);
    let mut bytes = digest(&bytes).to_vec();
    put32(&mut bytes, POLICY_COMMAND_CODE);
    put32(&mut bytes, UNSEAL);
    digest(&bytes)
}

/// The exact PCR state a sealed object's sole release policy requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcrPolicy {
    pub selection: PcrSelection,
    pub pcr_digest: [u8; 32],
}
impl PcrPolicy {
    pub fn digest(&self) -> [u8; 32] {
        policy_digest(self.selection, &self.pcr_digest)
    }
}

/// A sealed object's TPM2B_PUBLIC and TPM2B_PRIVATE contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedObject {
    pub public: Vec<u8>,
    pub private: Vec<u8>,
}

/// Refuse any sealed public area other than a SHA-256 keyed-hash object
/// with `SEALED_ATTRIBUTES` and exactly `policy` as its authPolicy.
pub fn validate_sealed_public(public: &[u8], policy: &[u8; 32]) -> Result<(), String> {
    let mut public = Reader(public);
    if public.u16()? != 8
        || public.u16()? != SHA256
        || public.u32()? != SEALED_ATTRIBUTES
        || public.blob()? != policy
        || public.u16()? != ALG_NULL
        || public.blob()?.len() != 32
    {
        return Err("sealed TPM object does not have the fixed PCR-only policy".into());
    }
    public.end()
}

pub struct Client<T: Transport> {
    transport: T,
    handles: Vec<u32>,
}
impl<T: Transport> Drop for Client<T> {
    fn drop(&mut self) {
        while let Some(handle) = self.handles.pop() {
            let _ = self.call(FLUSH_CONTEXT, &[handle], None, &[], false);
        }
    }
}
impl<T: Transport> Client<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            handles: Vec::new(),
        }
    }

    /// Transient objects and sessions this client still flushes on drop.
    pub fn owned_handles(&self) -> usize {
        self.handles.len()
    }

    /// One command; `auth` adds a single empty-password or policy session.
    /// A returned handle is owned until `flush`, Unseal or drop.
    pub fn call(
        &mut self,
        code: u32,
        handles: &[u32],
        auth: Option<u32>,
        parameters: &[u8],
        returns_handle: bool,
    ) -> Result<(Option<u32>, Vec<u8>), String> {
        let mut area = Vec::new();
        if let Some(auth) = auth {
            put32(&mut area, auth);
            if auth == PASSWORD {
                put_blob(&mut area, &[])?;
            } else {
                put_blob(&mut area, &random()?)?;
            }
            area.push(0);
            put_blob(&mut area, &[])?;
        }
        let area_size = u32::try_from(area.len()).map_err(|_| "oversized TPM authorization")?;
        // Sized and checked before `parameters`, which may carry a secret,
        // is copied, so no step after the copy can fail.
        let length = 10
            + 4 * handles.len()
            + if auth.is_some() { 4 + area.len() } else { 0 }
            + parameters.len();
        if length > MAX_PACKET {
            return Err("oversized TPM command".into());
        }
        let size = u32::try_from(length).map_err(|_| "oversized TPM command")?;
        let mut command = Vec::with_capacity(length);
        put16(
            &mut command,
            if auth.is_some() {
                SESSIONS
            } else {
                NO_SESSIONS
            },
        );
        put32(&mut command, size);
        put32(&mut command, code);
        for handle in handles {
            put32(&mut command, *handle);
        }
        if auth.is_some() {
            put32(&mut command, area_size);
            command.extend_from_slice(&area);
        }
        command.extend_from_slice(parameters);
        let response = self.transport.exchange(&command);
        zero(&mut command);
        let mut response = response?;
        let result = (|| {
            let mut reader = Reader(&response);
            let tag = reader.u16()?;
            if reader.u32()? as usize != response.len() || response.len() > MAX_PACKET {
                return Err("invalid TPM response size".into());
            }
            let rc = reader.u32()?;
            if rc != 0 {
                if tag != NO_SESSIONS || !reader.0.is_empty() {
                    return Err("malformed TPM error response".into());
                }
                return Err(format!("TPM command {code:#x} refused: {rc:#x}"));
            }
            if tag
                != if auth.is_some() {
                    SESSIONS
                } else {
                    NO_SESSIONS
                }
            {
                return Err("invalid TPM response tag".into());
            }
            let handle = if returns_handle {
                let handle = reader.u32()?;
                let kind = if code == START_AUTH_SESSION { 3 } else { 0x80 };
                if handle >> 24 != kind {
                    return Err("unexpected TPM handle class".into());
                }
                self.handles.push(handle);
                Some(handle)
            } else {
                None
            };
            let parameters = if let Some(auth) = auth {
                let size = reader.u32()? as usize;
                let parameters = reader.take(size)?;
                let nonce = reader.blob()?;
                if (auth == PASSWORD && !nonce.is_empty())
                    || (auth != PASSWORD && nonce.len() != 32)
                {
                    return Err("invalid TPM response nonce".into());
                }
                let flags = reader.u8()?;
                if flags != u8::from(auth == PASSWORD) || !reader.blob()?.is_empty() {
                    return Err("unexpected TPM authorization response".into());
                }
                reader.end()?;
                parameters.to_vec()
            } else {
                reader.0.to_vec()
            };
            Ok((handle, parameters))
        })();
        zero(&mut response);
        result
    }

    /// Flush an owned handle; it stays owned if the TPM does not confirm.
    pub fn flush(&mut self, handle: u32) -> Result<(), String> {
        let (_, out) = self.call(FLUSH_CONTEXT, &[handle], None, &[], false)?;
        Reader(&out).end()?;
        self.handles.retain(|value| *value != handle);
        Ok(())
    }

    /// The deterministic ECC P-256 restricted storage primary under an
    /// empty-authorization owner hierarchy, personalized by `unique.x`.
    pub fn storage_primary(
        &mut self,
        personalization: Option<&[u8; 32]>,
    ) -> Result<(u32, Vec<u8>), String> {
        let mut public = Vec::new();
        put16(&mut public, 0x23); // ECC
        put16(&mut public, SHA256);
        put32(&mut public, 0x30472); // fixed, generated, restricted decrypt parent
        put_blob(&mut public, &[])?;
        for value in [6, 128, 0x43, ALG_NULL, 3, ALG_NULL] {
            put16(&mut public, value);
        }
        let prefix_len = public.len();
        put_blob(
            &mut public,
            personalization.map_or(&[][..], |value| value.as_slice()),
        )?;
        put_blob(&mut public, &[])?;
        let mut parameters = Vec::new();
        put_blob(&mut parameters, &[0, 0, 0, 0])?;
        put_blob(&mut parameters, &public)?;
        put_blob(&mut parameters, &[])?;
        put32(&mut parameters, 0);
        let (handle, out) =
            self.call(CREATE_PRIMARY, &[OWNER], Some(PASSWORD), &parameters, true)?;
        let mut reader = Reader(&out);
        let returned_public = reader.blob()?;
        if returned_public.get(..prefix_len) != public.get(..prefix_len) {
            return Err("TPM changed the storage parent template".into());
        }
        let mut unique = Reader(
            returned_public
                .get(prefix_len..)
                .ok_or("short TPM parent")?,
        );
        let x = unique.blob()?;
        let y = unique.blob()?;
        if !(1..=32).contains(&x.len()) || !(1..=32).contains(&y.len()) {
            return Err("invalid TPM parent public point".into());
        }
        unique.end()?;
        creation(&mut reader)?;
        let name = reader.blob()?;
        check_name(returned_public, name)?;
        reader.end()?;
        Ok((handle.ok_or("missing TPM parent handle")?, name.to_vec()))
    }

    /// The personalized primary, refused if the TPM derives the same Name
    /// as the unpersonalized one (which it retires first).
    pub fn bound_storage_primary(&mut self, binding: &[u8; 32]) -> Result<u32, String> {
        let (unbound, unbound_name) = self.storage_primary(None)?;
        self.flush(unbound)?;
        let (bound, bound_name) = self.storage_primary(Some(binding))?;
        if bound_name == unbound_name {
            return Err("TPM ignored storage primary personalization".into());
        }
        Ok(bound)
    }

    /// The selected SHA-256 PCR values in ascending order. Whether a value
    /// is acceptable (for example, measured at all) is the caller's policy.
    pub fn read_pcrs(&mut self, selection: PcrSelection) -> Result<Vec<[u8; 32]>, String> {
        let (_, out) = self.call(PCR_READ, &[], None, &selection.marshal(), false)?;
        let mut reader = Reader(&out);
        reader.u32()?; // update counter; PolicyPCR closes the read/use race
        if reader.u32()? != 1 || reader.u16()? != SHA256 {
            return Err("TPM returned another PCR bank".into());
        }
        let width = reader.u8()?;
        if !(3..=4).contains(&width) {
            return Err("unsupported TPM PCR selection width".into());
        }
        let mask = reader.take(usize::from(width))?;
        if mask.get(..2) != Some(selection.mask().to_le_bytes().as_slice())
            || mask
                .get(2..)
                .is_none_or(|tail| tail.iter().any(|byte| *byte != 0))
            || reader.u32()? != selection.count()
        {
            return Err("TPM did not return the complete SHA-256 PCR selection".into());
        }
        let mut values = Vec::new();
        for _ in 0..selection.count() {
            let value = reader
                .blob()?
                .try_into()
                .map_err(|_| "TPM returned a malformed PCR value")?;
            values.push(value);
        }
        reader.end()?;
        Ok(values)
    }

    /// One of PCRs 0 through 15, the `PcrSelection` range.
    pub fn read_pcr(&mut self, index: u8) -> Result<[u8; 32], String> {
        self.read_pcrs(PcrSelection::new(pcr_bit(index)?)?)?
            .first()
            .copied()
            .ok_or_else(|| "missing PCR value".into())
    }

    /// Extend one of PCRs 0 through 15 with the SHA-256 `event`. A refused
    /// or malformed reply leaves the PCR state uncertain; this never retries.
    pub fn extend_pcr(&mut self, index: u8, event: &[u8; 32]) -> Result<(), String> {
        pcr_bit(index)?;
        let index = u32::from(index);
        let mut parameters = Vec::with_capacity(38);
        put32(&mut parameters, 1);
        put16(&mut parameters, SHA256);
        parameters.extend_from_slice(event);
        let (_, out) = self.call(PCR_EXTEND, &[index], Some(PASSWORD), &parameters, false)?;
        Reader(&out).end()
    }

    /// A policy session (or trial) satisfying `policy`, checked against the
    /// TPM's own PolicyGetDigest. The session is owned by this client.
    pub fn policy_session(&mut self, policy: &PcrPolicy, trial: bool) -> Result<u32, String> {
        let mut parameters = Vec::new();
        put_blob(&mut parameters, &random()?)?;
        put_blob(&mut parameters, &[])?;
        parameters.push(if trial { TRIAL_SESSION } else { POLICY_SESSION });
        put16(&mut parameters, ALG_NULL);
        put16(&mut parameters, SHA256);
        let (handle, out) =
            self.call(START_AUTH_SESSION, &[NULL, NULL], None, &parameters, true)?;
        let handle = handle.ok_or("missing TPM session handle")?;
        let mut reader = Reader(&out);
        if reader.blob()?.len() != 32 {
            return Err("invalid TPM session nonce".into());
        }
        reader.end()?;
        let mut parameters = Vec::new();
        put_blob(&mut parameters, &policy.pcr_digest)?;
        parameters.extend_from_slice(&policy.selection.marshal());
        let (_, out) = self.call(POLICY_PCR, &[handle], None, &parameters, false)?;
        Reader(&out).end()?;
        let (_, out) = self.call(
            POLICY_COMMAND_CODE,
            &[handle],
            None,
            &UNSEAL.to_be_bytes(),
            false,
        )?;
        Reader(&out).end()?;
        let (_, out) = self.call(POLICY_GET_DIGEST, &[handle], None, &[], false)?;
        let mut reader = Reader(&out);
        if reader.blob()? != policy.digest() {
            return Err("TPM policy digest mismatch".into());
        }
        reader.end()?;
        Ok(handle)
    }

    /// Seal `payload` under `policy` beneath the storage primary, bound to
    /// `binding` when given. `payload` is zeroed as soon as it is marshaled
    /// into the Create command, and on every return path.
    pub fn seal_object(
        mut self,
        policy: &PcrPolicy,
        binding: Option<&[u8; 32]>,
        payload: &mut [u8],
    ) -> Result<SealedObject, String> {
        if payload.is_empty() || payload.len() > MAX_SEALED_PAYLOAD {
            zero(payload);
            return Err("sealed TPM payload must hold 1 to 128 bytes".into());
        }
        let sealed = (|| {
            self.policy_session(policy, true)?;
            let parent = match binding {
                Some(binding) => self.bound_storage_primary(binding)?,
                None => self.storage_primary(None)?.0,
            };
            let mut public = Vec::new();
            put16(&mut public, 8);
            put16(&mut public, SHA256);
            put32(&mut public, SEALED_ATTRIBUTES);
            put_blob(&mut public, &policy.digest())?;
            put16(&mut public, ALG_NULL);
            put_blob(&mut public, &[])?;
            // TPM2B_SENSITIVE_CREATE: an empty userAuth, then the payload.
            let mut sensitive = Vec::with_capacity(4 + payload.len());
            put_blob(&mut sensitive, &[])?;
            let marshaled = put_blob(&mut sensitive, payload);
            zero(payload);
            if let Err(error) = marshaled {
                zero(&mut sensitive);
                return Err(error);
            }
            let mut parameters = Vec::with_capacity(2 + sensitive.len() + 2 + public.len() + 6);
            let marshaled = put_blob(&mut parameters, &sensitive)
                .and_then(|()| put_blob(&mut parameters, &public))
                .and_then(|()| put_blob(&mut parameters, &[]));
            zero(&mut sensitive);
            if let Err(error) = marshaled {
                zero(&mut parameters);
                return Err(error);
            }
            put32(&mut parameters, 0);
            let result = self.call(CREATE, &[parent], Some(PASSWORD), &parameters, false);
            zero(&mut parameters);
            let (_, out) = result?;
            let mut reader = Reader(&out);
            let private = reader.blob()?.to_vec();
            let public = reader.blob()?.to_vec();
            creation(&mut reader)?;
            reader.end()?;
            validate_sealed_public(&public, &policy.digest())?;
            Ok(SealedObject { public, private })
        })();
        zero(payload);
        sealed
    }

    /// Load and unseal an object sealed by `seal_object` under the same
    /// policy and binding. The caller owns zeroing the returned payload.
    pub fn unseal_object(
        mut self,
        policy: &PcrPolicy,
        binding: Option<&[u8; 32]>,
        public: &[u8],
        private: &[u8],
    ) -> Result<Vec<u8>, String> {
        validate_sealed_public(public, &policy.digest())?;
        let (parent, _) = self.storage_primary(binding)?;
        let handle = self.load(parent, public, private)?;
        let session = self.policy_session(policy, false)?;
        let (_, mut out) = self.call(UNSEAL, &[handle], Some(session), &[], false)?;
        // Unseal consumed the session: continueSession was clear.
        self.handles.retain(|handle| *handle != session);
        let result = (|| {
            let mut reader = Reader(&out);
            let payload = reader.blob()?;
            reader.end()?;
            if payload.is_empty() || payload.len() > MAX_SEALED_PAYLOAD {
                return Err("invalid unsealed TPM payload length".into());
            }
            Ok(payload.to_vec())
        })();
        zero(&mut out);
        result
    }

    /// Load a sealed pair under the storage primary for `binding`, then
    /// flush the object and the primary. The TPM refuses a private area
    /// whose integrity does not verify under that parent; nothing is
    /// unsealed, and the public area's format is the caller's to check.
    pub fn load_and_flush(
        &mut self,
        binding: Option<&[u8; 32]>,
        public: &[u8],
        private: &[u8],
    ) -> Result<(), String> {
        let (parent, _) = self.storage_primary(binding)?;
        let loaded = self
            .load(parent, public, private)
            .and_then(|handle| self.flush(handle));
        let flushed = self.flush(parent);
        loaded.and(flushed)
    }

    /// TPM2_Load under `parent`, checking the returned Name.
    fn load(&mut self, parent: u32, public: &[u8], private: &[u8]) -> Result<u32, String> {
        let mut parameters = Vec::new();
        put_blob(&mut parameters, private)?;
        put_blob(&mut parameters, public)?;
        let (handle, out) = self.call(LOAD, &[parent], Some(PASSWORD), &parameters, true)?;
        let handle = handle.ok_or("missing sealed object handle")?;
        let mut reader = Reader(&out);
        check_name(public, reader.blob()?)?;
        reader.end()?;
        Ok(handle)
    }
}

/// Zero a buffer that held secret material. `black_box` keeps the stores
/// observable so they are not elided before the buffer is freed; this is
/// best effort in safe Rust and does not reach copies the allocator or
/// the kernel made.
pub fn zero(bytes: &mut [u8]) {
    bytes.fill(0);
    std::hint::black_box(bytes);
}

/// The selection bit of one of PCRs 0 through 15.
fn pcr_bit(index: u8) -> Result<u16, String> {
    1u16.checked_shl(u32::from(index))
        .ok_or_else(|| "only static PCRs 0..15 are supported".into())
}

/// TPM2B_CREATION_DATA, the creation hash and an owner creation ticket.
pub fn creation(reader: &mut Reader<'_>) -> Result<(), String> {
    reader.blob()?; // creation data
    if reader.blob()?.len() != 32 || reader.u16()? != 0x8021 || reader.u32()? != OWNER {
        return Err("invalid TPM creation ticket".into());
    }
    // The hierarchy proof uses the TPM implementation's hash, not nameAlg.
    if !matches!(reader.blob()?.len(), 20 | 32 | 48 | 64) {
        return Err("invalid TPM creation digest".into());
    }
    Ok(())
}

/// A SHA-256 TPM Name must hash exactly this public area.
pub fn check_name(public: &[u8], name: &[u8]) -> Result<(), String> {
    let mut expected = SHA256.to_be_bytes().to_vec();
    expected.extend_from_slice(&digest(public));
    if name != expected {
        return Err("TPM object name mismatch".into());
    }
    Ok(())
}

fn random() -> Result<[u8; 32], String> {
    let mut bytes = [0; 32];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|e| e.to_string())?;
    Ok(bytes)
}

pub fn put16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

pub fn put32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// A TPM2B field: a u16 length, then the bytes.
pub fn put_blob(out: &mut Vec<u8>, value: &[u8]) -> Result<(), String> {
    put16(
        out,
        u16::try_from(value.len()).map_err(|_| "oversized TPM field")?,
    );
    out.extend_from_slice(value);
    Ok(())
}

/// A bounded big-endian reader over the unread remainder.
pub struct Reader<'a>(pub &'a [u8]);
impl<'a> Reader<'a> {
    pub fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let head = self.0.get(..count).ok_or("truncated TPM field")?;
        self.0 = self.0.get(count..).ok_or("truncated TPM field")?;
        Ok(head)
    }

    pub fn u8(&mut self) -> Result<u8, String> {
        self.take(1)?
            .first()
            .copied()
            .ok_or_else(|| "missing TPM byte".into())
    }

    pub fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().map_err(|_| "short TPM u16")?,
        ))
    }

    pub fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().map_err(|_| "short TPM u32")?,
        ))
    }

    pub fn blob(&mut self) -> Result<&'a [u8], String> {
        let size = self.u16()?;
        self.take(size as usize)
    }

    pub fn end(&self) -> Result<(), String> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err("trailing TPM data".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

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

    type Sent = Rc<RefCell<Vec<Vec<u8>>>>;

    /// Replies with one fixed packet and records every command.
    struct Fixed {
        reply: Vec<u8>,
        sent: Sent,
    }
    impl Fixed {
        fn client(reply: Vec<u8>) -> (Client<Self>, Sent) {
            let sent = Rc::new(RefCell::new(Vec::new()));
            let client = Client::new(Self {
                reply,
                sent: sent.clone(),
            });
            (client, sent)
        }
    }
    impl Transport for Fixed {
        fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
            self.sent.borrow_mut().push(command.to_vec());
            Ok(self.reply.clone())
        }
    }

    struct NoIo;
    impl Transport for NoIo {
        fn exchange(&mut self, _: &[u8]) -> Result<Vec<u8>, String> {
            panic!("refusal reached the TPM");
        }
    }

    /// The policy digest is a persisted contract: callers store sealed
    /// objects whose authPolicy it must reproduce.
    #[test]
    fn policy_digest_matches_the_tpm2_policy_pcr_then_command_code_formula() {
        let policy = PcrPolicy {
            selection: PcrSelection::new(1 << 7).unwrap(),
            pcr_digest: [6; 32],
        };
        assert_eq!(
            hex(&policy.digest()),
            "ee2145ba1675359be7e16f37ac26258720226007456eed056b0b16ede976c3e9"
        );
        assert_eq!(policy.digest(), policy_digest(policy.selection, &[6; 32]));
        let mut joined = [1; 64];
        joined[32..].fill(2);
        assert_eq!(pcr_digest(&[[1; 32], [2; 32]]), digest(&joined));
    }

    #[test]
    fn selections_marshal_one_sha256_bank_and_refuse_bad_masks() {
        assert_eq!(
            PcrSelection::new(0x8081).unwrap().marshal(),
            [0, 0, 0, 1, 0, 0x0b, 3, 0x81, 0x80, 0]
        );
        assert_eq!(
            PcrSelection::new(0x1210).unwrap().marshal(),
            [0, 0, 0, 1, 0, 0x0b, 3, 0x10, 0x12, 0]
        );
        assert!(PcrSelection::new(0).is_err());
        assert!(PcrSelection::new(0x01ff).is_err());
        assert_eq!(PcrSelection::new(0x807f).unwrap().count(), 8);
        assert_eq!(PcrSelection::new(0x8081).unwrap().mask(), 0x8081);
    }

    #[test]
    fn pcr_read_matches_the_mask_independent_of_selection_width() {
        let selection = PcrSelection::new(1 << 7).unwrap();
        for width in [3, 4] {
            let mut parameters = Vec::new();
            put32(&mut parameters, 0);
            put32(&mut parameters, 1);
            put16(&mut parameters, SHA256);
            parameters.push(width);
            parameters.push(0x80);
            parameters.extend(std::iter::repeat_n(0, usize::from(width) - 1));
            put32(&mut parameters, 1);
            put_blob(&mut parameters, &[4; 32]).unwrap();
            let good = response(NO_SESSIONS, 0, &parameters);
            assert_eq!(
                Fixed::client(good.clone()).0.read_pcrs(selection).unwrap(),
                [[4; 32]]
            );
            let mut extra = good.clone();
            extra[21] |= 1; // extra PCR selected without an extra digest
            assert!(Fixed::client(extra).0.read_pcrs(selection).is_err());
            let mut short = good.clone();
            short.pop();
            short[5] -= 1;
            let at = short.len() - 33;
            short[at] = 31;
            assert!(Fixed::client(short).0.read_pcrs(selection).is_err());
            // A zero value is returned: refusing it is the caller's policy.
            let at = good.len() - 32;
            let mut zero = good;
            zero[at..].fill(0);
            assert_eq!(
                Fixed::client(zero).0.read_pcrs(selection).unwrap(),
                [[0; 32]]
            );
        }
    }

    /// td-boot's selector PCR 11 measurement sends these bytes; its own
    /// tests pin them too.
    #[test]
    fn single_pcr_read_and_extend_send_the_selector_command_bytes() {
        let mut body = Vec::new();
        put32(&mut body, 7);
        body.extend_from_slice(&PcrSelection::new(1 << 11).unwrap().marshal());
        put32(&mut body, 1);
        put_blob(&mut body, &[3; 32]).unwrap();
        let (mut client, sent) = Fixed::client(response(NO_SESSIONS, 0, &body));
        assert_eq!(client.read_pcr(11).unwrap(), [3; 32]);
        assert_eq!(
            sent.borrow()[0],
            [0x80, 1, 0, 0, 0, 20, 0, 0, 1, 0x7e, 0, 0, 0, 1, 0, 0x0b, 3, 0, 8, 0]
        );
        assert!(client.read_pcr(16).is_err());
        assert_eq!(sent.borrow().len(), 1);

        let extended = [
            0x80, 2, 0, 0, 0, 19, 0, 0, 0, 0, // session response
            0, 0, 0, 0, 0, 0, 1, 0, 0, // no parameters; empty password response
        ];
        let (mut client, sent) = Fixed::client(extended.to_vec());
        client.extend_pcr(11, &[0x5c; 32]).unwrap();
        let mut command = vec![
            0x80, 2, 0, 0, 0, 65, 0, 0, 1, 0x82, // PCR_Extend
            0, 0, 0, 11, 0, 0, 0, 9, // handle, authorization size
            0x40, 0, 0, 9, 0, 0, 0, 0, 0, // empty password session
            0, 0, 0, 1, 0, 0x0b, // one SHA-256 digest
        ];
        command.extend_from_slice(&[0x5c; 32]);
        assert_eq!(sent.borrow().as_slice(), [command]);
        assert!(client.extend_pcr(16, &[0; 32]).is_err());
        assert_eq!(sent.borrow().len(), 1);

        // Anything but the exact empty reply leaves the extension uncertain.
        let mut trailing = extended.to_vec();
        trailing[5] += 1;
        trailing[13] = 1;
        trailing.insert(14, 0);
        let mut no_continue = extended.to_vec();
        no_continue[16] = 0;
        for reply in [
            trailing,
            no_continue,
            response(NO_SESSIONS, 0x922, &[]),
            response(NO_SESSIONS, 0, &[]),
            extended[..18].to_vec(),
        ] {
            assert!(Fixed::client(reply).0.extend_pcr(11, &[0; 32]).is_err());
        }
    }

    #[test]
    fn malformed_transport_replies_are_refused() {
        let good = response(NO_SESSIONS, 0, &[]);
        let read = |reply: Vec<u8>| Fixed::client(reply).0.call(PCR_READ, &[], None, &[], false);
        for size in 0..good.len() {
            assert!(read(good[..size].to_vec()).is_err());
        }
        let mut oversized = good.clone();
        oversized.push(0);
        assert!(read(oversized).is_err());
        assert!(Fixed::client(good.clone())
            .0
            .call(UNSEAL, &[], Some(PASSWORD), &[], false)
            .is_err());
        let mut wrong_tag = good.clone();
        wrong_tag[0] = 0;
        assert!(read(wrong_tag).is_err());
        assert_eq!(
            read(response(NO_SESSIONS, 0x101, &[])).unwrap_err(),
            "TPM command 0x17e refused: 0x101"
        );
        assert!(read(response(NO_SESSIONS, 0x101, &[0])).is_err());
        let mut packet = vec![0; MAX_PACKET + 1];
        packet[..2].copy_from_slice(&NO_SESSIONS.to_be_bytes());
        packet[2..6].copy_from_slice(&(MAX_PACKET as u32 + 1).to_be_bytes());
        assert_eq!(read(packet).unwrap_err(), "invalid TPM response size");
        let (mut client, sent) = Fixed::client(good);
        assert!(client
            .call(PCR_READ, &[], None, &[0; MAX_PACKET], false)
            .is_err());
        assert!(sent.borrow().is_empty());
        // A returned handle of the wrong class is refused and never owned.
        let (mut client, _) =
            Fixed::client(response(NO_SESSIONS, 0, &0x0300_0000u32.to_be_bytes()));
        assert!(client.call(LOAD, &[], None, &[], true).is_err());
        assert_eq!(client.owned_handles(), 0);
    }

    #[test]
    fn owned_handles_are_flushed_once_and_kept_when_flush_fails() {
        struct Flushes {
            refuse: bool,
            loads: u32,
            flushed: Rc<RefCell<Vec<u32>>>,
        }
        impl Transport for Flushes {
            fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
                let code = u32::from_be_bytes(command[6..10].try_into().unwrap());
                if code == FLUSH_CONTEXT {
                    let handle = u32::from_be_bytes(command[10..14].try_into().unwrap());
                    self.flushed.borrow_mut().push(handle);
                    let rc = if self.refuse { 0x101 } else { 0 };
                    return Ok(response(NO_SESSIONS, rc, &[]));
                }
                self.loads += 1;
                let handle = 0x8000_0000 + self.loads;
                Ok(response(NO_SESSIONS, 0, &handle.to_be_bytes()))
            }
        }
        for refuse in [false, true] {
            let flushed = Rc::new(RefCell::new(Vec::new()));
            let mut client = Client::new(Flushes {
                refuse,
                loads: 0,
                flushed: flushed.clone(),
            });
            let first = client.call(LOAD, &[], None, &[], true).unwrap().0.unwrap();
            let second = client.call(LOAD, &[], None, &[], true).unwrap().0.unwrap();
            assert_eq!(client.owned_handles(), 2);
            assert_eq!(client.flush(first).is_ok(), !refuse);
            assert_eq!(client.owned_handles(), if refuse { 2 } else { 1 });
            drop(client);
            assert_eq!(
                *flushed.borrow(),
                if refuse {
                    vec![first, second, first]
                } else {
                    vec![first, second]
                }
            );
        }
    }

    #[test]
    fn primary_binding_is_marshaled_and_ignored_personalization_is_refused() {
        struct Reply {
            binding: [u8; 32],
            ignore: bool,
            creates: Rc<Cell<usize>>,
            flushes: Rc<Cell<usize>>,
        }
        impl Transport for Reply {
            fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
                let code = u32::from_be_bytes(command[6..10].try_into().unwrap());
                if code == FLUSH_CONTEXT {
                    self.flushes.set(self.flushes.get() + 1);
                    return Ok(response(NO_SESSIONS, 0, &[]));
                }
                assert_eq!(code, CREATE_PRIMARY);
                let mut input = Reader(&command[10..]);
                assert_eq!(input.u32().unwrap(), OWNER);
                let auth_size = input.u32().unwrap() as usize;
                input.take(auth_size).unwrap();
                assert_eq!(input.blob().unwrap(), [0, 0, 0, 0]);
                let public = input.blob().unwrap();
                let prefix = [
                    0, 0x23, 0, 0x0b, 0, 3, 4, 0x72, 0, 0, 0, 6, 0, 128, 0, 0x43, 0, 0x10, 0, 3, 0,
                    0x10,
                ];
                assert_eq!(&public[..22], prefix);
                let bound = self.creates.get() == 1;
                let mut unique = Reader(&public[22..]);
                assert_eq!(
                    unique.blob().unwrap(),
                    if bound { &self.binding[..] } else { &[] }
                );
                assert!(unique.blob().unwrap().is_empty());
                unique.end().unwrap();
                assert!(input.blob().unwrap().is_empty());
                assert_eq!(input.u32().unwrap(), 0);
                input.end().unwrap();
                self.creates.set(self.creates.get() + 1);
                let mut returned = prefix.to_vec();
                put_blob(
                    &mut returned,
                    &[if bound && !self.ignore { 2 } else { 1 }; 32],
                )
                .unwrap();
                put_blob(&mut returned, &[3; 32]).unwrap();
                let mut parameters = Vec::new();
                put_blob(&mut parameters, &returned).unwrap();
                put_blob(&mut parameters, &[]).unwrap();
                put_blob(&mut parameters, &[4; 32]).unwrap();
                put16(&mut parameters, 0x8021);
                put32(&mut parameters, OWNER);
                put_blob(&mut parameters, &[5; 32]).unwrap();
                let mut name = SHA256.to_be_bytes().to_vec();
                name.extend_from_slice(&digest(&returned));
                put_blob(&mut parameters, &name).unwrap();
                let mut body = (0x8000_0000 + u32::from(bound)).to_be_bytes().to_vec();
                put32(&mut body, parameters.len() as u32);
                body.extend_from_slice(&parameters);
                body.extend_from_slice(&[0, 0, 1, 0, 0]);
                Ok(response(SESSIONS, 0, &body))
            }
        }
        for binding in [[0; 32], [9; 32]] {
            for ignore in [false, true] {
                let creates = Rc::new(Cell::new(0));
                let flushes = Rc::new(Cell::new(0));
                let mut client = Client::new(Reply {
                    binding,
                    ignore,
                    creates: creates.clone(),
                    flushes: flushes.clone(),
                });
                let result = client.bound_storage_primary(&binding);
                assert_eq!(result.is_err(), ignore);
                if ignore {
                    assert!(result.unwrap_err().contains("ignored"));
                }
                assert_eq!(creates.get(), 2);
                assert_eq!(flushes.get(), 1);
                drop(client);
                assert_eq!(flushes.get(), 2);
            }
        }
    }

    #[test]
    fn sealed_payload_and_public_area_are_bounded_before_any_tpm_io() {
        let policy = PcrPolicy {
            selection: PcrSelection::new(1 << 7).unwrap(),
            pcr_digest: [6; 32],
        };
        for size in [0, MAX_SEALED_PAYLOAD + 1] {
            let mut payload = vec![0x42; size];
            assert!(Client::new(NoIo)
                .seal_object(&policy, None, &mut payload)
                .is_err());
            assert!(payload.iter().all(|byte| *byte == 0));
        }
        let mut public = Vec::new();
        put16(&mut public, 8);
        put16(&mut public, SHA256);
        put32(&mut public, SEALED_ATTRIBUTES);
        put_blob(&mut public, &policy.digest()).unwrap();
        put16(&mut public, ALG_NULL);
        put_blob(&mut public, &[8; 32]).unwrap();
        validate_sealed_public(&public, &policy.digest()).unwrap();
        let mut user_with_auth = public.clone();
        user_with_auth[7] |= 0x40; // would permit password authorization
        let mut trailing = public.clone();
        trailing.push(0);
        let other = PcrPolicy {
            pcr_digest: [7; 32],
            ..policy
        };
        for (bytes, expected) in [
            (&user_with_auth, policy.digest()),
            (&trailing, policy.digest()),
            (&public, other.digest()),
        ] {
            assert!(validate_sealed_public(bytes, &expected).is_err());
        }
        for size in 0..public.len() {
            assert!(validate_sealed_public(&public[..size], &policy.digest()).is_err());
        }
        assert!(Client::new(NoIo)
            .unseal_object(&other, None, &public, &[1])
            .is_err());
    }

    /// TPM_RC_VALUE on parameter 1, TPM_RC_POLICY_FAIL on session 1, and
    /// TPM_RC_INTEGRITY on parameter 1 for a private area that fails Load.
    const RC_VALUE: u32 = 0x1c4;
    const RC_POLICY_FAIL: u32 = 0x99d;
    const RC_INTEGRITY: u32 = 0x19f;

    /// A scripted TPM with PCR state. A real policy session's PolicyPCR
    /// refuses a composite that differs from its PCRs, a trial session
    /// takes the caller's, and Unseal refuses a session whose digest is
    /// not the loaded object's authPolicy, as a TPM does.
    struct Tpm {
        codes: Vec<u32>,
        reads: Vec<u16>,
        pcrs: [[u8; 32]; 16],
        next: u32,
        sessions: Vec<(u32, bool, [u8; 32])>,
        objects: Vec<(u32, Vec<u8>, Vec<u8>)>,
    }

    #[derive(Clone)]
    struct Scripted(Rc<RefCell<Tpm>>);
    impl Scripted {
        fn new() -> Self {
            let mut pcrs = [[0; 32]; 16];
            pcrs[4] = [0x44; 32];
            pcrs[7] = [0x47; 32];
            pcrs[9] = [0x49; 32];
            Self(Rc::new(RefCell::new(Tpm {
                codes: Vec::new(),
                reads: Vec::new(),
                pcrs,
                next: 0,
                sessions: Vec::new(),
                objects: Vec::new(),
            })))
        }

        fn client(&self) -> Client<Self> {
            Client::new(self.clone())
        }

        fn codes(&self) -> Vec<u32> {
            std::mem::take(&mut self.0.borrow_mut().codes)
        }
    }
    impl Transport for Scripted {
        fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
            Ok(self.0.borrow_mut().reply(command))
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
        name.extend_from_slice(&digest(public));
        name
    }

    impl Tpm {
        fn composite(&self, mask: u16) -> [u8; 32] {
            let values: Vec<[u8; 32]> = (0..16)
                .filter(|index| mask & (1 << index) != 0)
                .map(|index| self.pcrs[index])
                .collect();
            pcr_digest(&values)
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
            *state = digest(&bytes);
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
                    let event = input.take(32).unwrap();
                    input.end().unwrap();
                    let pcr = &mut self.pcrs[handle.unwrap() as usize];
                    *pcr = digest(&[&pcr[..], event].concat());
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
                    let mut public = template[..22].to_vec();
                    put_blob(&mut public, &digest(template)).unwrap();
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
                    put_blob(&mut public, &digest(data)).unwrap();
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

    const SEAL: &[u32] = &[
        START_AUTH_SESSION,
        POLICY_PCR,
        POLICY_COMMAND_CODE,
        POLICY_GET_DIGEST,
        CREATE_PRIMARY,
        CREATE,
        FLUSH_CONTEXT,
        FLUSH_CONTEXT,
    ];
    const UNSEAL_CODES: &[u32] = &[
        CREATE_PRIMARY,
        LOAD,
        START_AUTH_SESSION,
        POLICY_PCR,
        POLICY_COMMAND_CODE,
        POLICY_GET_DIGEST,
        UNSEAL,
        FLUSH_CONTEXT,
        FLUSH_CONTEXT,
    ];

    #[test]
    fn scripted_tpm_seals_unseals_and_refuses_a_mismatched_session() {
        let tpm = Scripted::new();
        let selection = PcrSelection::new(1 << 7).unwrap();
        let values = tpm.client().read_pcrs(selection).unwrap();
        assert_eq!(values, [[0x47; 32]]);
        assert_eq!(tpm.codes(), [PCR_READ]);
        let policy = PcrPolicy {
            selection,
            pcr_digest: pcr_digest(&values),
        };
        let mut payload = [0x21; 32];
        let sealed = tpm
            .client()
            .seal_object(&policy, None, &mut payload)
            .unwrap();
        assert_eq!(payload, [0; 32]);
        assert_eq!(tpm.codes(), SEAL);
        validate_sealed_public(&sealed.public, &policy.digest()).unwrap();
        let unsealed = tpm
            .client()
            .unseal_object(&policy, None, &sealed.public, &sealed.private)
            .unwrap();
        assert_eq!(unsealed, [0x21; 32]);
        assert_eq!(tpm.codes(), UNSEAL_CODES);
        assert!(tpm.0.borrow().sessions.is_empty());
        assert!(tpm.0.borrow().objects.is_empty());

        // The TPM, not only the client's own check, refuses an Unseal in a
        // session that satisfied another policy.
        let mut client = tpm.client();
        let (parent, _) = client.storage_primary(None).unwrap();
        let mut parameters = Vec::new();
        put_blob(&mut parameters, &sealed.private).unwrap();
        put_blob(&mut parameters, &sealed.public).unwrap();
        let (object, _) = client
            .call(LOAD, &[parent], Some(PASSWORD), &parameters, true)
            .unwrap();
        let other_selection = PcrSelection::new(1 << 4).unwrap();
        let other = PcrPolicy {
            selection: other_selection,
            pcr_digest: pcr_digest(&client.read_pcrs(other_selection).unwrap()),
        };
        let session = client.policy_session(&other, false).unwrap();
        assert_eq!(
            client
                .call(UNSEAL, &[object.unwrap()], Some(session), &[], false)
                .unwrap_err(),
            "TPM command 0x15e refused: 0x99d"
        );
        drop(client);
        assert!(tpm.0.borrow().sessions.is_empty());
        assert!(tpm.0.borrow().objects.is_empty());
        tpm.codes();

        // Once the PCR moves, the real session's PolicyPCR refuses and no
        // Unseal is sent.
        tpm.client().extend_pcr(7, &[1; 32]).unwrap();
        assert!(tpm
            .client()
            .unseal_object(&policy, None, &sealed.public, &sealed.private)
            .unwrap_err()
            .contains("0x17f refused"));
        assert!(!tpm.codes().contains(&UNSEAL));
    }

    #[test]
    fn load_and_flush_loads_the_pair_and_leaves_no_handle() {
        let tpm = Scripted::new();
        let policy = PcrPolicy {
            selection: PcrSelection::new(1 << 12).unwrap(),
            pcr_digest: pcr_digest(&[[0; 32]]),
        };
        let mut payload = [0x21; 32];
        let sealed = tpm
            .client()
            .seal_object(&policy, None, &mut payload)
            .unwrap();
        tpm.codes();
        let mut client = tpm.client();
        client
            .load_and_flush(None, &sealed.public, &sealed.private)
            .unwrap();
        assert_eq!(client.owned_handles(), 0);
        assert_eq!(
            tpm.codes(),
            [CREATE_PRIMARY, LOAD, FLUSH_CONTEXT, FLUSH_CONTEXT]
        );
        assert!(tpm.0.borrow().objects.is_empty());

        let mut private = sealed.private.clone();
        private[2] ^= 1;
        assert_eq!(
            client
                .load_and_flush(None, &sealed.public, &private)
                .unwrap_err(),
            "TPM command 0x157 refused: 0x19f"
        );
        assert_eq!(client.owned_handles(), 0);
        assert_eq!(tpm.codes(), [CREATE_PRIMARY, LOAD, FLUSH_CONTEXT]);
    }

    #[test]
    fn scripted_tpm_extends_then_reads_a_pcr() {
        let tpm = Scripted::new();
        let mut client = tpm.client();
        client.extend_pcr(11, &[0x5c; 32]).unwrap();
        let mut expected = [0; 64];
        expected[32..].fill(0x5c);
        assert_eq!(client.read_pcr(11).unwrap(), digest(&expected));
        assert!(client.extend_pcr(16, &[0; 32]).is_err());
        assert!(client.read_pcr(16).is_err());
        assert_eq!(tpm.codes(), [PCR_EXTEND, PCR_READ]);
    }

    /// The disk protector's shape: PCR 12 is sealed at the literal zero it
    /// holds before the cap, never read, and the cap blocks release.
    #[test]
    fn a_policy_may_include_a_literal_pcr_value_that_is_never_read() {
        let tpm = Scripted::new();
        let mut values = tpm
            .client()
            .read_pcrs(PcrSelection::new(1 << 4 | 1 << 9).unwrap())
            .unwrap();
        values.push([0; 32]);
        let policy = PcrPolicy {
            selection: PcrSelection::new(1 << 4 | 1 << 9 | 1 << 12).unwrap(),
            pcr_digest: pcr_digest(&values),
        };
        let mut payload = [0x33; 64];
        let sealed = tpm
            .client()
            .seal_object(&policy, None, &mut payload)
            .unwrap();
        let unseal = || {
            tpm.client()
                .unseal_object(&policy, None, &sealed.public, &sealed.private)
        };
        assert_eq!(unseal().unwrap(), [0x33; 64]);
        tpm.client().extend_pcr(12, &[0x5c; 32]).unwrap();
        assert!(unseal().unwrap_err().contains("0x17f refused"));
        assert!(tpm.0.borrow().reads.iter().all(|mask| *mask == 0x0210));
        let codes = tpm.codes();
        assert_eq!(codes.iter().filter(|code| **code == UNSEAL).count(), 1);
    }
}
