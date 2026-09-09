//! TPM 2.0 sealing and public-only ES256 verification.

use crate::crypto;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};

const NO_SESSIONS: u16 = 0x8001;
const SESSIONS: u16 = 0x8002;
const OWNER: u32 = 0x4000_0001;
const NULL: u32 = 0x4000_0007;
const PASSWORD: u32 = 0x4000_0009;
const SHA256: u16 = 0x000b;
const O_NOFOLLOW: i32 = 0o400000;
const TRIAL_SESSION: u8 = 0x03;
const ALG_NULL: u16 = 0x0010;
const CREATE_PRIMARY: u32 = 0x131;
const CREATE: u32 = 0x153;
const LOAD: u32 = 0x157;
const UNSEAL: u32 = 0x15e;
const POLICY_COMMAND_CODE: u32 = 0x16c;
const POLICY_PCR: u32 = 0x17f;
const PCR_READ: u32 = 0x17e;
const START_AUTH_SESSION: u32 = 0x176;
const FLUSH_CONTEXT: u32 = 0x165;
const POLICY_GET_DIGEST: u32 = 0x189;
const LOAD_EXTERNAL: u32 = 0x167;
const VERIFY_SIGNATURE: u32 = 0x177;
const SEALED_ATTRIBUTES: u32 = 0x492;
pub const MAX_PACKET: usize = 4096;
const MAX_BOUND_KEY: usize = 4096;

pub trait Transport {
    fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String>;
}

pub struct Device(File);
impl Device {
    pub fn open() -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NOFOLLOW)
            .open("/dev/tpmrm0")
            .map_err(|e| format!("open TPM resource manager: {e}"))?;
        if !file
            .metadata()
            .map_err(|e| e.to_string())?
            .file_type()
            .is_char_device()
        {
            return Err("TPM resource manager is not a character device".into());
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pcrs(u16);
impl Pcrs {
    pub fn parse(value: &str) -> Result<Self, String> {
        let mut mask = 0u16;
        for item in value.split(',') {
            if item.is_empty() || !item.bytes().all(|b| b.is_ascii_digit()) {
                return Err("PCR selection must list numbers from 0 through 15".into());
            }
            let index = item.parse::<u32>().map_err(|_| "invalid PCR number")?;
            let bit = 1u16
                .checked_shl(index)
                .ok_or("only static PCRs 0..15 are supported")?;
            if mask & bit != 0 {
                return Err("duplicate PCR selection".into());
            }
            mask |= bit;
        }
        Self::from_mask(mask)
    }
    fn from_mask(mask: u16) -> Result<Self, String> {
        if mask == 0 {
            return Err("empty PCR policy".into());
        }
        if mask.count_ones() > 8 {
            return Err("select at most eight PCRs per store".into());
        }
        Ok(Self(mask))
    }
    fn selection(self) -> Vec<u8> {
        let mut bytes = Vec::new();
        put32(&mut bytes, 1);
        put16(&mut bytes, SHA256);
        bytes.push(3);
        bytes.extend_from_slice(&self.0.to_le_bytes());
        bytes.push(0);
        bytes
    }
}

#[derive(Clone)]
pub struct SealedKey {
    pub uid: u32,
    pcrs: Pcrs,
    pcr_digest: [u8; 32],
    public: Vec<u8>,
    private: Vec<u8>,
}
impl SealedKey {
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let mut bytes = b"TDTPM001".to_vec();
        put32(&mut bytes, self.uid);
        put16(&mut bytes, self.pcrs.0);
        bytes.extend_from_slice(&self.pcr_digest);
        put_blob(&mut bytes, &self.public)?;
        put_blob(&mut bytes, &self.private)?;
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_PACKET {
            return Err("oversized sealed TPM key".into());
        }
        let mut reader = Reader(bytes);
        if reader.take(8)? != b"TDTPM001" {
            return Err("invalid sealed TPM key format".into());
        }
        let uid = reader.u32()?;
        let pcrs = Pcrs::from_mask(reader.u16()?)?;
        let pcr_digest = reader
            .take(32)?
            .try_into()
            .map_err(|_| "short PCR digest")?;
        let public = reader.blob()?.to_vec();
        let private = reader.blob()?.to_vec();
        if private.is_empty() {
            return Err("empty sealed TPM private area".into());
        }
        reader.end()?;
        let result = Self {
            uid,
            pcrs,
            pcr_digest,
            public,
            private,
        };
        result.validate_public()?;
        Ok(result)
    }
    fn policy_digest(&self) -> [u8; 32] {
        let mut bytes = vec![0; 32];
        put32(&mut bytes, POLICY_PCR);
        bytes.extend_from_slice(&self.pcrs.selection());
        bytes.extend_from_slice(&self.pcr_digest);
        let mut bytes = crypto::digest(&bytes).to_vec();
        put32(&mut bytes, POLICY_COMMAND_CODE);
        put32(&mut bytes, UNSEAL);
        crypto::digest(&bytes)
    }
    fn validate_public(&self) -> Result<(), String> {
        let mut public = Reader(&self.public);
        if public.u16()? != 8
            || public.u16()? != SHA256
            || public.u32()? != SEALED_ATTRIBUTES
            || public.blob()? != self.policy_digest()
            || public.u16()? != ALG_NULL
            || public.blob()?.len() != 32
        {
            return Err("sealed TPM object does not have the fixed PCR-only policy".into());
        }
        public.end()
    }
}

/// A sealed child whose storage primary is personalized by enrollment metadata.
pub struct BoundKey {
    binding: [u8; 32],
    key: SealedKey,
}

impl BoundKey {
    /// Unverified disk metadata until a successful unseal checks the payload.
    pub fn uid(&self) -> u32 {
        self.key.uid
    }

    /// Structural agreement only; TPM Load must still authenticate this object.
    pub fn require_binding(&self, uid: u32, binding: &[u8; 32]) -> Result<(), String> {
        if self.uid() != uid || &self.binding != binding {
            return Err("sealed key and enrollment metadata disagree".into());
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let mut bytes = b"TDBOUND1".to_vec();
        bytes.extend_from_slice(&self.binding);
        put_blob(&mut bytes, &self.key.encode()?)?;
        if bytes.len() > MAX_BOUND_KEY {
            return Err("oversized bound TPM key".into());
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_BOUND_KEY {
            return Err("oversized bound TPM key".into());
        }
        let mut reader = Reader(bytes);
        if reader.take(8)? != b"TDBOUND1" {
            return Err("invalid bound TPM key format".into());
        }
        let binding = reader
            .take(32)?
            .try_into()
            .map_err(|_| "short metadata binding")?;
        let key = SealedKey::decode(reader.blob()?)?;
        reader.end()?;
        Ok(Self { binding, key })
    }
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

    /// Verify a digest with a public-only P-256 key. This grants no store access.
    pub fn verify_es256(
        &mut self,
        x: &[u8; 32],
        y: &[u8; 32],
        digest: &[u8; 32],
        r: &[u8; 32],
        s: &[u8; 32],
    ) -> Result<(), String> {
        let mut public = Vec::with_capacity(88);
        put16(&mut public, 0x23); // ECC
        put16(&mut public, SHA256);
        put32(&mut public, 0x40040); // unrestricted signing, userWithAuth
        put_blob(&mut public, &[])?;
        for value in [ALG_NULL, 0x18, SHA256, 3, ALG_NULL] {
            put16(&mut public, value);
        }
        put_blob(&mut public, x)?;
        put_blob(&mut public, y)?;
        let mut parameters = Vec::new();
        put_blob(&mut parameters, &[])?; // public only
        put_blob(&mut parameters, &public)?;
        put32(&mut parameters, NULL);
        let (handle, out) = self.call(LOAD_EXTERNAL, &[], None, &parameters, true)?;
        let handle = handle.ok_or("missing TPM verification handle")?;
        let verified = (|| {
            let mut reader = Reader(&out);
            check_name(&public, reader.blob()?)?;
            reader.end()?;
            let mut signature = Vec::new();
            put_blob(&mut signature, digest)?;
            put16(&mut signature, 0x18); // ECDSA
            put16(&mut signature, SHA256);
            put_blob(&mut signature, r)?;
            put_blob(&mut signature, s)?;
            let (_, out) = self.call(VERIFY_SIGNATURE, &[handle], None, &signature, false)?;
            let mut ticket = Reader(&out);
            if ticket.u16()? != 0x8022 || ticket.u32()? != NULL || !ticket.blob()?.is_empty() {
                return Err("invalid public-only TPM verification ticket".into());
            }
            ticket.end()
        })();
        // Repeated assertions must not exhaust the TPM's transient object slots.
        let flushed = self
            .call(FLUSH_CONTEXT, &[handle], None, &[], false)
            .and_then(|(_, out)| Reader(&out).end());
        if flushed.is_ok() {
            self.handles.retain(|value| *value != handle);
        }
        match (verified, flushed) {
            (Err(primary), Err(cleanup)) => Err(format!(
                "{primary}; verification object cleanup failed: {cleanup}"
            )),
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    fn call(
        &mut self,
        code: u32,
        handles: &[u32],
        auth: Option<u32>,
        parameters: &[u8],
        returns_handle: bool,
    ) -> Result<(Option<u32>, Vec<u8>), String> {
        let mut command = Vec::new();
        put16(
            &mut command,
            if auth.is_some() {
                SESSIONS
            } else {
                NO_SESSIONS
            },
        );
        put32(&mut command, 0);
        put32(&mut command, code);
        for handle in handles {
            put32(&mut command, *handle);
        }
        if let Some(auth) = auth {
            let mut area = Vec::new();
            put32(&mut area, auth);
            if auth == PASSWORD {
                put_blob(&mut area, &[])?;
            } else {
                put_blob(&mut area, &random()?)?;
            }
            area.push(0);
            put_blob(&mut area, &[])?;
            put32(
                &mut command,
                u32::try_from(area.len()).map_err(|_| "oversized TPM authorization")?,
            );
            command.extend_from_slice(&area);
        }
        command.extend_from_slice(parameters);
        if command.len() > MAX_PACKET {
            return Err("oversized TPM command".into());
        }
        let size = u32::try_from(command.len()).map_err(|_| "oversized TPM command")?;
        command
            .get_mut(2..6)
            .ok_or("missing TPM command size")?
            .copy_from_slice(&size.to_be_bytes());
        let response = self.transport.exchange(&command);
        command.fill(0);
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
        response.fill(0);
        result
    }

    fn parent(&mut self, binding: Option<&[u8; 32]>) -> Result<(u32, Vec<u8>), String> {
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
            binding.map_or(&[][..], |value| value.as_slice()),
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
        if !(1..=32).contains(&unique.blob()?.len()) || !(1..=32).contains(&unique.blob()?.len()) {
            return Err("invalid TPM parent public point".into());
        }
        unique.end()?;
        creation(&mut reader)?;
        let name = reader.blob()?;
        check_name(returned_public, name)?;
        reader.end()?;
        Ok((handle.ok_or("missing TPM parent handle")?, name.to_vec()))
    }

    fn bound_parent(&mut self, binding: &[u8; 32]) -> Result<u32, String> {
        let (unbound, unbound_name) = self.parent(None)?;
        let (_, out) = self.call(FLUSH_CONTEXT, &[unbound], None, &[], false)?;
        Reader(&out).end()?;
        self.handles.retain(|handle| *handle != unbound);
        let (bound, bound_name) = self.parent(Some(binding))?;
        if bound_name == unbound_name {
            return Err("TPM ignored storage primary personalization".into());
        }
        Ok(bound)
    }

    fn snapshot(&mut self, pcrs: Pcrs) -> Result<[u8; 32], String> {
        let selection = pcrs.selection();
        let (_, out) = self.call(PCR_READ, &[], None, &selection, false)?;
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
        if mask.get(..2) != Some(pcrs.0.to_le_bytes().as_slice())
            || mask
                .get(2..)
                .is_none_or(|tail| tail.iter().any(|byte| *byte != 0))
            || reader.u32()? != pcrs.0.count_ones()
        {
            return Err("TPM did not return the complete SHA-256 PCR selection".into());
        }
        let mut values = Vec::new();
        for _ in 0..pcrs.0.count_ones() {
            let digest = reader.blob()?;
            if digest.len() != 32 || digest.iter().all(|byte| *byte == 0) {
                return Err("selected PCR is missing or unmeasured".into());
            }
            values.extend_from_slice(digest);
        }
        reader.end()?;
        Ok(crypto::digest(&values))
    }

    fn policy(&mut self, key: &SealedKey, trial: bool) -> Result<u32, String> {
        let mut parameters = Vec::new();
        put_blob(&mut parameters, &random()?)?;
        put_blob(&mut parameters, &[])?;
        parameters.push(if trial { TRIAL_SESSION } else { 1 });
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
        put_blob(&mut parameters, &key.pcr_digest)?;
        parameters.extend_from_slice(&key.pcrs.selection());
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
        if reader.blob()? != key.policy_digest() {
            return Err("TPM policy digest mismatch".into());
        }
        reader.end()?;
        Ok(handle)
    }

    pub fn seal(self, uid: u32, pcrs: Pcrs, master: &[u8; 32]) -> Result<SealedKey, String> {
        self.seal_inner(uid, pcrs, master, None)
    }

    /// The digest must bind the complete canonical enrollment and recovery policy.
    pub fn seal_bound(
        self,
        uid: u32,
        pcrs: Pcrs,
        master: &[u8; 32],
        binding: &[u8; 32],
    ) -> Result<BoundKey, String> {
        let key = BoundKey {
            binding: *binding,
            key: self.seal_inner(uid, pcrs, master, Some(binding))?,
        };
        key.encode()?;
        Ok(key)
    }

    fn seal_inner(
        mut self,
        uid: u32,
        pcrs: Pcrs,
        master: &[u8; 32],
        binding: Option<&[u8; 32]>,
    ) -> Result<SealedKey, String> {
        let pcr_digest = self.snapshot(pcrs)?;
        let mut key = SealedKey {
            uid,
            pcrs,
            pcr_digest,
            public: Vec::new(),
            private: Vec::new(),
        };
        self.policy(&key, true)?;
        let parent = match binding {
            Some(binding) => self.bound_parent(binding)?,
            None => self.parent(None)?.0,
        };
        let mut public = Vec::new();
        put16(&mut public, 8);
        put16(&mut public, SHA256);
        put32(&mut public, SEALED_ATTRIBUTES);
        put_blob(&mut public, &key.policy_digest())?;
        put16(&mut public, ALG_NULL);
        put_blob(&mut public, &[])?;
        let mut sensitive = vec![0, 0];
        let mut payload = uid.to_be_bytes().to_vec();
        payload.extend_from_slice(master);
        put_blob(&mut sensitive, &payload)?;
        payload.fill(0);
        let mut parameters = Vec::new();
        put_blob(&mut parameters, &sensitive)?;
        sensitive.fill(0);
        put_blob(&mut parameters, &public)?;
        put_blob(&mut parameters, &[])?;
        put32(&mut parameters, 0);
        let result = self.call(CREATE, &[parent], Some(PASSWORD), &parameters, false);
        parameters.fill(0);
        let (_, out) = result?;
        let mut reader = Reader(&out);
        key.private = reader.blob()?.to_vec();
        key.public = reader.blob()?.to_vec();
        creation(&mut reader)?;
        reader.end()?;
        key.validate_public()?;
        Ok(key)
    }

    pub fn unseal(self, key: &SealedKey) -> Result<[u8; 32], String> {
        self.unseal_inner(key, None)
    }

    /// Metadata integrity only: the caller must authorize the user before this call.
    pub fn unseal_bound(self, key: &BoundKey, binding: &[u8; 32]) -> Result<[u8; 32], String> {
        if binding != &key.binding {
            return Err("sealed token metadata digest mismatch".into());
        }
        self.unseal_inner(&key.key, Some(binding))
    }

    fn unseal_inner(
        mut self,
        key: &SealedKey,
        binding: Option<&[u8; 32]>,
    ) -> Result<[u8; 32], String> {
        key.validate_public()?;
        let (parent, _) = self.parent(binding)?;
        let mut parameters = Vec::new();
        put_blob(&mut parameters, &key.private)?;
        put_blob(&mut parameters, &key.public)?;
        let (handle, out) = self.call(LOAD, &[parent], Some(PASSWORD), &parameters, true)?;
        let handle = handle.ok_or("missing sealed object handle")?;
        let mut reader = Reader(&out);
        check_name(&key.public, reader.blob()?)?;
        reader.end()?;
        let session = self.policy(key, false)?;
        let (_, mut out) = self.call(UNSEAL, &[handle], Some(session), &[], false)?;
        self.handles.retain(|handle| *handle != session);
        let result = (|| {
            let mut reader = Reader(&out);
            let mut payload = Reader(reader.blob()?);
            if payload.u32()? != key.uid {
                return Err("sealed TPM key belongs to another user".into());
            }
            let master = payload
                .take(32)?
                .try_into()
                .map_err(|_| "invalid unsealed key length")?;
            payload.end()?;
            reader.end()?;
            Ok(master)
        })();
        out.fill(0);
        result
    }
}

fn creation(reader: &mut Reader<'_>) -> Result<(), String> {
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
fn check_name(public: &[u8], name: &[u8]) -> Result<(), String> {
    let mut expected = SHA256.to_be_bytes().to_vec();
    expected.extend_from_slice(&crypto::digest(public));
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
fn put16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}
fn put32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}
fn put_blob(out: &mut Vec<u8>, value: &[u8]) -> Result<(), String> {
    put16(
        out,
        u16::try_from(value.len()).map_err(|_| "oversized TPM field")?,
    );
    out.extend_from_slice(value);
    Ok(())
}
struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let head = self.0.get(..count).ok_or("truncated TPM field")?;
        self.0 = self.0.get(count..).ok_or("truncated TPM field")?;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, String> {
        self.take(1)?
            .first()
            .copied()
            .ok_or_else(|| "missing TPM byte".into())
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().map_err(|_| "short TPM u16")?,
        ))
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().map_err(|_| "short TPM u32")?,
        ))
    }
    fn blob(&mut self) -> Result<&'a [u8], String> {
        let size = self.u16()?;
        self.take(size as usize)
    }
    fn end(&self) -> Result<(), String> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err("trailing TPM data".into())
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    // Disposable virtual-token signer, compiled only into the test harness.
    pub(crate) struct SigningKey {
        client: Client<Device>,
        handle: u32,
        pub(crate) cose: Vec<u8>,
    }

    impl SigningKey {
        pub(crate) fn new() -> Self {
            Self::in_hierarchy(NULL, &random().unwrap())
        }

        pub(crate) fn persistent(unique: &[u8; 32]) -> Self {
            Self::in_hierarchy(OWNER, unique)
        }

        fn in_hierarchy(hierarchy: u32, unique: &[u8; 32]) -> Self {
            let mut client = Client::new(Device::open().unwrap());
            let mut public = Vec::new();
            put16(&mut public, 0x23);
            put16(&mut public, SHA256);
            put32(&mut public, 0x40072); // fixed, generated, unrestricted signer
            put_blob(&mut public, &[]).unwrap();
            for value in [ALG_NULL, 0x18, SHA256, 3, ALG_NULL] {
                put16(&mut public, value);
            }
            let prefix = public.len();
            put_blob(&mut public, unique).unwrap();
            put_blob(&mut public, &[]).unwrap();
            let mut parameters = Vec::new();
            put_blob(&mut parameters, &[0; 4]).unwrap();
            put_blob(&mut parameters, &public).unwrap();
            put_blob(&mut parameters, &[]).unwrap();
            put32(&mut parameters, 0);
            let (handle, out) = client
                .call(CREATE_PRIMARY, &[hierarchy], Some(PASSWORD), &parameters, true)
                .unwrap();
            let mut reader = Reader(&out);
            let returned = reader.blob().unwrap();
            assert_eq!(&returned[..prefix], &public[..prefix]);
            let mut unique = Reader(&returned[prefix..]);
            let mut cose = vec![0xa5, 1, 2, 3, 0x26, 0x20, 1, 0x21, 0x58, 0x20];
            for coordinate in 0..2 {
                let value = unique.blob().unwrap();
                assert!((1..=32).contains(&value.len()));
                if coordinate == 1 {
                    cose.extend_from_slice(&[0x22, 0x58, 0x20]);
                }
                // COSE coordinates are fixed-width, big-endian integers.
                cose.resize(cose.len() + 32 - value.len(), 0);
                cose.extend_from_slice(value);
            }
            unique.end().unwrap();
            let creation_data = reader.blob().unwrap();
            assert_eq!(reader.blob().unwrap(), crypto::digest(creation_data));
            assert_eq!(reader.u16().unwrap(), 0x8021);
            assert_eq!(reader.u32().unwrap(), hierarchy);
            assert!(matches!(reader.blob().unwrap().len(), 20 | 32 | 48 | 64));
            check_name(returned, reader.blob().unwrap()).unwrap();
            reader.end().unwrap();
            Self {
                client,
                handle: handle.unwrap(),
                cose,
            }
        }

        pub(crate) fn sign(&mut self, digest: &[u8; 32]) -> Vec<u8> {
            let mut parameters = Vec::new();
            put_blob(&mut parameters, digest).unwrap();
            put16(&mut parameters, ALG_NULL);
            put16(&mut parameters, 0x8024); // empty HASHCHECK ticket: unrestricted key
            put32(&mut parameters, NULL);
            put_blob(&mut parameters, &[]).unwrap();
            let (_, out) = self
                .client
                .call(0x15d, &[self.handle], Some(PASSWORD), &parameters, false) // Sign
                .unwrap();
            let mut reader = Reader(&out);
            assert_eq!(reader.u16().unwrap(), 0x18);
            assert_eq!(reader.u16().unwrap(), SHA256);
            let mut integers = Vec::new();
            for _ in 0..2 {
                let value = reader.blob().unwrap();
                assert!((1..=32).contains(&value.len()));
                let first = value.iter().position(|byte| *byte != 0)
                    .expect("TPM returned a zero ECDSA component");
                let value = &value[first..];
                let pad = usize::from(value[0] & 0x80 != 0);
                integers.extend_from_slice(&[2, (value.len() + pad) as u8]);
                if pad != 0 {
                    integers.push(0);
                }
                integers.extend_from_slice(value);
            }
            reader.end().unwrap();
            let mut der = vec![0x30, integers.len() as u8];
            der.extend_from_slice(&integers);
            der
        }
    }

    pub(crate) struct Socket(UnixStream);
    impl Transport for Socket {
        fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
            self.0.write_all(command).map_err(|e| e.to_string())?;
            let mut header = [0; 10];
            self.0.read_exact(&mut header).map_err(|e| e.to_string())?;
            let size = u32::from_be_bytes(header[2..6].try_into().unwrap()) as usize;
            if !(10..=MAX_PACKET).contains(&size) {
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
    pub(crate) struct Emulator {
        child: Child,
        socket: PathBuf,
    }
    impl Emulator {
        pub(crate) fn start(state: &Path) -> Self {
            std::fs::create_dir_all(state).unwrap();
            let socket = state.join("socket");
            let _ = std::fs::remove_file(&socket);
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
            let mut result = Self { child, socket };
            let deadline = Instant::now() + Duration::from_secs(10);
            while !result.socket.exists() {
                assert!(result.child.try_wait().unwrap().is_none(), "swtpm exited");
                assert!(Instant::now() < deadline, "swtpm startup timeout");
                std::thread::sleep(Duration::from_millis(10));
            }
            result
        }
        pub(crate) fn client(&self) -> Client<Socket> {
            let socket = UnixStream::connect(&self.socket).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            Client::new(Socket(socket))
        }
        pub(crate) fn extend(&self, digest: &[u8; 32]) {
            let mut parameters = 1u32.to_be_bytes().to_vec();
            put16(&mut parameters, SHA256);
            parameters.extend_from_slice(digest);
            self.client()
                .call(0x182, &[7], Some(PASSWORD), &parameters, false)
                .unwrap();
        }
    }
    impl Drop for Emulator {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    pub(crate) fn fixture(uid: u32) -> Vec<u8> {
        let mut key = SealedKey {
            uid,
            pcrs: Pcrs::parse("7").unwrap(),
            pcr_digest: [6; 32],
            public: Vec::new(),
            private: vec![7; 64],
        };
        put16(&mut key.public, 8);
        put16(&mut key.public, SHA256);
        put32(&mut key.public, SEALED_ATTRIBUTES);
        let policy = key.policy_digest();
        put_blob(&mut key.public, &policy).unwrap();
        put16(&mut key.public, ALG_NULL);
        put_blob(&mut key.public, &[8; 32]).unwrap();
        key.encode().unwrap()
    }

    #[test]
    fn selections_and_malformed_envelopes_are_refused() {
        for value in ["", "16", "32", "0,0", "-1", "7,", "7 8", " 7"] {
            assert!(Pcrs::parse(value).is_err(), "{value}");
        }
        assert_eq!(Pcrs::parse("0,7,15").unwrap().0, 0x8081);
        for size in 0..100 {
            assert!(SealedKey::decode(&vec![0; size]).is_err());
        }
        assert!(SealedKey::decode(&vec![0; MAX_PACKET + 1]).is_err());
    }

    #[test]
    fn too_many_pcrs_are_refused_before_contacting_the_tpm() {
        assert!(Pcrs::parse("0,1,2,3,4,5,6,7,8").is_err());
        assert!(Pcrs::parse("0,1,2,3,4,5,6,15").is_ok());
    }

    #[test]
    fn pcr_read_matches_the_mask_independent_of_selection_width() {
        struct Reply(Vec<u8>);
        impl Transport for Reply {
            fn exchange(&mut self, _: &[u8]) -> Result<Vec<u8>, String> {
                Ok(self.0.clone())
            }
        }
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
            let mut response = NO_SESSIONS.to_be_bytes().to_vec();
            put32(&mut response, 10 + parameters.len() as u32);
            put32(&mut response, 0);
            response.extend_from_slice(&parameters);
            assert_eq!(
                Client::new(Reply(response.clone()))
                    .snapshot(Pcrs::parse("7").unwrap())
                    .unwrap(),
                crypto::digest(&[4; 32])
            );
            response[21] |= 1; // extra PCR selected without an extra digest
            assert!(Client::new(Reply(response))
                .snapshot(Pcrs::parse("7").unwrap())
                .is_err());
        }
    }

    #[test]
    fn malformed_transport_replies_and_policy_downgrades_are_refused() {
        struct Reply(Vec<u8>);
        impl Transport for Reply {
            fn exchange(&mut self, _: &[u8]) -> Result<Vec<u8>, String> {
                Ok(self.0.clone())
            }
        }
        let mut good = NO_SESSIONS.to_be_bytes().to_vec();
        put32(&mut good, 10);
        put32(&mut good, 0);
        for size in 0..good.len() {
            assert!(Client::new(Reply(good[..size].to_vec()))
                .call(PCR_READ, &[], None, &[], false)
                .is_err());
        }
        let mut oversized = good.clone();
        oversized.push(0);
        assert!(Client::new(Reply(oversized))
            .call(PCR_READ, &[], None, &[], false)
            .is_err());
        assert!(Client::new(Reply(good.clone()))
            .call(UNSEAL, &[], Some(PASSWORD), &[], false)
            .is_err());
        let mut wrong_tag = good;
        wrong_tag[0] = 0;
        assert!(Client::new(Reply(wrong_tag))
            .call(PCR_READ, &[], None, &[], false)
            .is_err());
        let key = SealedKey::decode(&fixture(1000)).unwrap();
        let mut downgraded = key.clone();
        downgraded.public[7] |= 0x40; // userWithAuth would permit password authorization
        assert!(SealedKey::decode(&downgraded.encode().unwrap()).is_err());
        let mut changed_pcr = key;
        changed_pcr.pcr_digest[0] ^= 1;
        assert!(SealedKey::decode(&changed_pcr.encode().unwrap()).is_err());
    }

    #[test]
    fn es256_rejects_malformed_names_tickets_and_failed_cleanup() {
        use std::cell::Cell;
        use std::rc::Rc;
        struct DeviceReply {
            fault: u8,
            flushes: Rc<Cell<usize>>,
        }
        impl Transport for DeviceReply {
            fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
                let code = u32::from_be_bytes(command[6..10].try_into().unwrap());
                let mut out = Vec::new();
                let mut rc = 0;
                match code {
                    LOAD_EXTERNAL => {
                        let mut request = Reader(&command[10..]);
                        assert!(request.blob().unwrap().is_empty());
                        let public = request.blob().unwrap();
                        assert_eq!(request.u32().unwrap(), NULL);
                        request.end().unwrap();
                        let mut name = SHA256.to_be_bytes().to_vec();
                        name.extend_from_slice(&crypto::digest(public));
                        if self.fault == 1 {
                            name[2] ^= 1;
                        }
                        put32(
                            &mut out,
                            if self.fault == 12 {
                                0x8100_0000
                            } else {
                                0x8000_0000
                            },
                        );
                        put_blob(&mut out, &name).unwrap();
                        if self.fault == 2 {
                            out.push(0);
                        }
                        if self.fault == 11 {
                            rc = 0x101;
                            out.clear();
                        }
                    }
                    VERIFY_SIGNATURE => {
                        put16(&mut out, if self.fault == 3 { 0x8021 } else { 0x8022 });
                        put32(&mut out, if self.fault == 4 { OWNER } else { NULL });
                        put_blob(&mut out, if self.fault == 5 { &[1] } else { &[] }).unwrap();
                        if self.fault == 6 {
                            out.push(0);
                        }
                        if matches!(self.fault, 8 | 10) {
                            rc = 0x2db;
                            out.clear();
                        }
                        if self.fault == 9 {
                            out.pop();
                        }
                    }
                    FLUSH_CONTEXT => {
                        self.flushes.set(self.flushes.get() + 1);
                        if matches!(self.fault, 7 | 10) && self.flushes.get() == 1 {
                            rc = 0x101;
                        }
                    }
                    _ => panic!("unexpected verification command {code:#x}"),
                }
                let mut response = NO_SESSIONS.to_be_bytes().to_vec();
                put32(&mut response, 10 + out.len() as u32);
                put32(&mut response, rc);
                response.extend_from_slice(&out);
                Ok(response)
            }
        }
        let [x, y, digest, r, s] = es256_fixture();
        for fault in 0..=12 {
            let flushes = Rc::new(Cell::new(0));
            let mut client = Client::new(DeviceReply {
                fault,
                flushes: flushes.clone(),
            });
            let result = client.verify_es256(&x, &y, &digest, &r, &s);
            assert_eq!(result.is_ok(), fault == 0, "fault {fault}");
            if fault == 10 {
                let error = result.unwrap_err();
                assert!(error.contains("0x177") && error.contains("0x165"));
            }
            let loaded = usize::from(fault < 11);
            assert_eq!(
                flushes.get(),
                loaded,
                "only admitted transient objects are retired"
            );
            assert_eq!(client.handles.len(), usize::from(matches!(fault, 7 | 10)));
            drop(client);
            assert_eq!(
                flushes.get(),
                if matches!(fault, 7 | 10) { 2 } else { loaded }
            );
        }
    }

    #[test]
    fn primary_binding_is_marshaled_and_ignored_personalization_is_refused() {
        use std::cell::Cell;
        use std::rc::Rc;
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
                    let mut response = NO_SESSIONS.to_be_bytes().to_vec();
                    put32(&mut response, 10);
                    put32(&mut response, 0);
                    return Ok(response);
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
                name.extend_from_slice(&crypto::digest(&returned));
                put_blob(&mut parameters, &name).unwrap();
                let mut response = SESSIONS.to_be_bytes().to_vec();
                put32(&mut response, (23 + parameters.len()) as u32);
                put32(&mut response, 0);
                put32(&mut response, 0x80000000 + u32::from(bound));
                put32(&mut response, parameters.len() as u32);
                response.extend_from_slice(&parameters);
                response.extend_from_slice(&[0, 0, 1, 0, 0]);
                Ok(response)
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
                let result = client.bound_parent(&binding);
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
    fn bound_key_codec_rejects_truncation_and_mismatched_metadata_before_io() {
        struct NoIo;
        impl Transport for NoIo {
            fn exchange(&mut self, _: &[u8]) -> Result<Vec<u8>, String> {
                panic!("metadata mismatch reached TPM");
            }
        }
        let key = BoundKey {
            binding: [9; 32],
            key: SealedKey::decode(&fixture(1000)).unwrap(),
        };
        let bytes = key.encode().unwrap();
        assert_eq!(BoundKey::decode(&bytes).unwrap().uid(), 1000);
        assert_eq!(BoundKey::decode(&bytes).unwrap().encode().unwrap(), bytes);
        for size in 0..bytes.len() {
            assert!(BoundKey::decode(&bytes[..size]).is_err());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(BoundKey::decode(&trailing).is_err());
        let mut bad_magic = bytes.clone();
        bad_magic[0] ^= 1;
        assert!(BoundKey::decode(&bad_magic).is_err());
        let mut oversized = bytes.clone();
        oversized.resize(MAX_BOUND_KEY + 1, 0);
        assert_eq!(
            BoundKey::decode(&oversized).err().unwrap(),
            "oversized bound TPM key"
        );
        assert!(Client::new(NoIo).unseal_bound(&key, &[8; 32]).is_err());
    }

    #[test]
    #[ignore = "requires explicitly supplied pinned host swtpm; never accesses hardware"]
    fn emulator_metadata_binding_survives_restart_and_refuses_substitution() {
        struct Directory(PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = std::env::temp_dir().join(format!("td-bound-key-oracle-{}", std::process::id()));
        assert!(!root.exists());
        let _directory = Directory(root.clone());
        let emulator = Emulator::start(&root);
        emulator.extend(&[7; 32]);
        let original = crypto::digest(b"test token metadata: primary A, recovery B, uid 1000");
        let key = emulator
            .client()
            .seal_bound(1000, Pcrs::parse("7").unwrap(), &[0x34; 32], &original)
            .unwrap();
        let bytes = key.encode().unwrap();
        assert_eq!(
            emulator.client().unseal_bound(&key, &original).unwrap(),
            [0x34; 32]
        );
        for changed in [crypto::digest(b"test substituted primary key"), [0; 32]] {
            let mut edited = bytes.clone();
            edited[8..40].copy_from_slice(&changed);
            let edited = BoundKey::decode(&edited).unwrap();
            assert!(emulator.client().unseal_bound(&edited, &changed).is_err());
        }
        let zero = emulator
            .client()
            .seal_bound(1000, Pcrs::parse("7").unwrap(), &[0x35; 32], &[0; 32])
            .unwrap();
        assert_eq!(
            emulator.client().unseal_bound(&zero, &[0; 32]).unwrap(),
            [0x35; 32]
        );
        assert!(emulator.client().unseal(&zero.key).is_err());
        // Removing the wrapper cannot select the old, empty-unique storage parent.
        assert!(emulator.client().unseal(&key.key).is_err());
        let mut edited = BoundKey::decode(&bytes).unwrap();
        edited.key.uid = 1001;
        assert!(emulator.client().unseal_bound(&edited, &original).is_err());
        assert_eq!(
            emulator.client().unseal_bound(&key, &original).unwrap(),
            [0x34; 32]
        );
        drop(emulator);
        let restarted = Emulator::start(&root);
        restarted.extend(&[7; 32]);
        let key = BoundKey::decode(&bytes).unwrap();
        assert_eq!(
            restarted.client().unseal_bound(&key, &original).unwrap(),
            [0x34; 32]
        );
        restarted.extend(&[8; 32]);
        assert!(restarted.client().unseal_bound(&key, &original).is_err());
        drop(restarted);
    }

    fn es256_fixture() -> [[u8; 32]; 5] {
        // Independently generated OpenSSL 3.5.7 P-256/SHA-256 signature.
        [
            "024b51d901d395bc26de4f967225248bc9a7df360209a932856be223d5e1559d",
            "22cc0e2fa5c56fcbbfc143e1ecd4009d1ff9ac33b9e047bb935ea5e563996c55",
            "4994c7bb96286fd9ef8d505d8ab3dea532624cf615bc59f44df535ada05dd5f9",
            "49298c57698034a7fb139bb67afd8019356759df90fc1d118f0ecfa66f0c463b",
            "8683952a24bd4b70eb95c04b1e0f560ffd94cfb31688836a20cfd7381b76983d",
        ]
        .map(|value| {
            std::array::from_fn(|i| u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).unwrap())
        })
    }

    #[test]
    #[ignore = "requires explicitly supplied pinned host swtpm; never accesses hardware"]
    fn emulator_es256_verifies_mutations_and_retires_handles() {
        let root = std::env::temp_dir().join(format!("td-es256-oracle-{}", std::process::id()));
        assert!(!root.exists());
        let emulator = Emulator::start(&root);
        let mut client = emulator.client();
        let [x, y, digest, r, s] = es256_fixture();
        for _ in 0..16 {
            client.verify_es256(&x, &y, &digest, &r, &s).unwrap();
            assert!(client.handles.is_empty());
        }
        for field in 0..5 {
            let mut values = [x, y, digest, r, s];
            values[field][0] ^= 1;
            let [x, y, digest, r, s] = values;
            assert!(
                client.verify_es256(&x, &y, &digest, &r, &s).is_err(),
                "field {field}"
            );
            assert!(client.handles.is_empty());
        }
        drop(client);
        drop(emulator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires explicitly supplied pinned host swtpm; never accesses hardware"]
    fn emulator_seals_reboots_and_refuses_changed_platform_or_tpm() {
        let root = std::env::temp_dir().join(format!("td-tpm-oracle-{}", std::process::id()));
        assert!(!root.exists());
        let state = root.join("first");
        let emulator = Emulator::start(&state);
        let pcrs = Pcrs::parse("7").unwrap();
        assert!(
            emulator.client().seal(1000, pcrs, &[42; 32]).is_err(),
            "zero PCR accepted"
        );
        emulator.extend(&[9; 32]);
        let sealed = emulator.client().seal(1000, pcrs, &[42; 32]).unwrap();
        let encoded = sealed.encode().unwrap();
        let sealed = SealedKey::decode(&encoded).unwrap();
        assert_eq!(emulator.client().unseal(&sealed).unwrap(), [42; 32]);
        for size in 0..encoded.len() {
            assert!(SealedKey::decode(&encoded[..size]).is_err());
        }
        let mut swapped_user = sealed.clone();
        swapped_user.uid = 1001;
        assert!(
            emulator.client().unseal(&swapped_user).is_err(),
            "UID substitution released key"
        );
        let mut corrupt = sealed.clone();
        corrupt.private[4] ^= 1;
        assert!(emulator.client().unseal(&corrupt).is_err());
        // TPM2_Shutdown(CLEAR) makes the persisted owner seed reusable at boot.
        emulator
            .client()
            .call(0x145, &[], None, &0u16.to_be_bytes(), false)
            .unwrap();
        drop(emulator);
        let emulator = Emulator::start(&state);
        emulator.extend(&[9; 32]);
        assert_eq!(emulator.client().unseal(&sealed).unwrap(), [42; 32]);
        emulator.extend(&[8; 32]);
        assert!(
            emulator.client().unseal(&sealed).is_err(),
            "changed PCR released key"
        );
        let other = Emulator::start(&root.join("other"));
        other.extend(&[9; 32]);
        assert!(
            other.client().unseal(&sealed).is_err(),
            "different TPM released key"
        );
        drop(other);
        drop(emulator);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn qemu_guard(case: &str) {
        assert!(std::fs::read_to_string("/proc/cmdline").unwrap()
            .split_whitespace().any(|word| word == "td.tpm-fixture=1"),
            "requires the explicitly selected disposable TPM guest");
        assert_eq!(std::fs::read_to_string("/case").unwrap(), case);
    }

    fn qemu_client() -> Client<Device> {
        Client::new(Device::open().unwrap())
    }

    pub(crate) fn qemu_extend(digest: &[u8; 32]) {
        let mut parameters = 1u32.to_be_bytes().to_vec();
        put16(&mut parameters, SHA256);
        parameters.extend_from_slice(digest);
        qemu_client().call(0x182, &[7], Some(PASSWORD), &parameters, false).unwrap();
    }

    fn qemu_disk(write: bool) -> File {
        let file = OpenOptions::new().read(true).write(write)
            .custom_flags(O_NOFOLLOW).open("/dev/vda").unwrap();
        assert!(file.metadata().unwrap().file_type().is_block_device());
        file
    }

    const QEMU_BINDING: [u8; 32] = [17; 32];
    const QEMU_KEY: [u8; 32] = [42; 32];
    const QEMU_DISK_MAGIC: &[u8; 8] = b"TDQTPM01";

    fn qemu_read_key() -> BoundKey {
        let mut disk = qemu_disk(false);
        let mut magic = [0; 8];
        disk.read_exact(&mut magic).unwrap();
        assert_eq!(&magic, QEMU_DISK_MAGIC);
        let mut length = [0; 4];
        disk.read_exact(&mut length).unwrap();
        let length = u32::from_be_bytes(length) as usize;
        assert!((1..=MAX_BOUND_KEY).contains(&length));
        let mut bytes = vec![0; length];
        disk.read_exact(&mut bytes).unwrap();
        let key = BoundKey::decode(&bytes).unwrap();
        key.require_binding(1000, &QEMU_BINDING).unwrap();
        qemu_extend(&[9; 32]);
        assert_eq!(qemu_client().snapshot(Pcrs::parse("7").unwrap()).unwrap(),
            key.key.pcr_digest, "cold boot did not reproduce the fixture PCR state");
        key
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable TPM and disk"]
    fn qemu_device_seals_to_persistent_state() {
        use std::io::{Seek, SeekFrom};
        qemu_guard("tpm-seal");
        qemu_extend(&[9; 32]);
        let sealed = qemu_client().seal_bound(1000, Pcrs::parse("7").unwrap(),
            &QEMU_KEY, &QEMU_BINDING).unwrap();
        assert_eq!(qemu_client().unseal_bound(&sealed, &QEMU_BINDING).unwrap(), QEMU_KEY);
        let encoded = sealed.encode().unwrap();
        let mut disk = qemu_disk(true);
        let mut empty = [0; 4096];
        disk.read_exact(&mut empty).unwrap();
        assert!(empty.iter().all(|byte| *byte == 0), "fixture disk is not fresh");
        disk.seek(SeekFrom::Start(0)).unwrap();
        disk.write_all(QEMU_DISK_MAGIC).unwrap();
        disk.write_all(&u32::try_from(encoded.len()).unwrap().to_be_bytes()).unwrap();
        disk.write_all(&encoded).unwrap();
        disk.sync_all().unwrap();
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable TPM and disk"]
    fn qemu_device_reopens_after_cold_boot() {
        qemu_guard("tpm-reopen");
        let sealed = qemu_read_key();
        assert_eq!(qemu_client().unseal_bound(&sealed, &QEMU_BINDING).unwrap(), QEMU_KEY);
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable TPM and disk"]
    fn qemu_device_refuses_changed_pcr() {
        qemu_guard("tpm-pcr");
        let sealed = qemu_read_key();
        assert_eq!(qemu_client().unseal_bound(&sealed, &QEMU_BINDING).unwrap(), QEMU_KEY);
        qemu_extend(&[8; 32]);
        assert_ne!(qemu_client().snapshot(Pcrs::parse("7").unwrap()).unwrap(),
            sealed.key.pcr_digest);
        let error = qemu_client().unseal_bound(&sealed, &QEMU_BINDING).unwrap_err();
        assert!(error.starts_with("TPM command 0x17f refused:"), "{error}");
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable TPM and disk"]
    fn qemu_device_refuses_another_tpm() {
        qemu_guard("tpm-other");
        let sealed = qemu_read_key();
        let error = qemu_client().unseal_bound(&sealed, &QEMU_BINDING).unwrap_err();
        assert!(error.starts_with("TPM command 0x157 refused:"), "{error}");
    }

}
