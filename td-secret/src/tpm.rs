//! td-secret's sealed-store formats and PCR policy over td-tpm's client,
//! and public-only ES256 verification.

use td_tpm::{check_name, put16, put32, put_blob, PcrPolicy, PcrSelection, Reader};
pub use td_tpm::{Device, Transport, MAX_PACKET};
use td_tpm::{ALG_NULL, NULL, SHA256};

const LOAD_EXTERNAL: u32 = 0x167;
const VERIFY_SIGNATURE: u32 = 0x177;
const MAX_BOUND_KEY: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pcrs(PcrSelection);
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
        Ok(Self(PcrSelection::new(mask)?))
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
        put16(&mut bytes, self.pcrs.0.mask());
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
    fn policy(&self) -> PcrPolicy {
        PcrPolicy {
            selection: self.pcrs.0,
            pcr_digest: self.pcr_digest,
        }
    }
    fn policy_digest(&self) -> [u8; 32] {
        self.policy().digest()
    }
    fn validate_public(&self) -> Result<(), String> {
        td_tpm::validate_sealed_public(&self.public, &self.policy_digest())
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

/// td-secret's operations on td-tpm's client; it owns no TPM state itself.
pub struct Client<T: Transport>(td_tpm::Client<T>);
impl<T: Transport> Client<T> {
    pub fn new(transport: T) -> Self {
        Self(td_tpm::Client::new(transport))
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
        let (handle, out) = self.0.call(LOAD_EXTERNAL, &[], None, &parameters, true)?;
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
            let (_, out) = self
                .0
                .call(VERIFY_SIGNATURE, &[handle], None, &signature, false)?;
            let mut ticket = Reader(&out);
            if ticket.u16()? != 0x8022 || ticket.u32()? != NULL || !ticket.blob()?.is_empty() {
                return Err("invalid public-only TPM verification ticket".into());
            }
            ticket.end()
        })();
        // Repeated assertions must not exhaust the TPM's transient object slots.
        let flushed = self.0.flush(handle);
        match (verified, flushed) {
            (Err(primary), Err(cleanup)) => Err(format!(
                "{primary}; verification object cleanup failed: {cleanup}"
            )),
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    /// The composite PCR digest a store seals to; an unmeasured PCR is refused.
    fn snapshot(&mut self, pcrs: Pcrs) -> Result<[u8; 32], String> {
        let values = self.0.read_pcrs(pcrs.0)?;
        if values
            .iter()
            .any(|value| value.iter().all(|byte| *byte == 0))
        {
            return Err("selected PCR is missing or unmeasured".into());
        }
        Ok(td_tpm::pcr_digest(&values))
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
        let policy = PcrPolicy {
            selection: pcrs.0,
            pcr_digest,
        };
        // The sealed payload binds the owner UID to the master.
        let mut payload = uid.to_be_bytes().to_vec();
        payload.extend_from_slice(master);
        let sealed = self.0.seal_object(&policy, binding, &mut payload);
        payload.fill(0);
        let sealed = sealed?;
        Ok(SealedKey {
            uid,
            pcrs,
            pcr_digest,
            public: sealed.public,
            private: sealed.private,
        })
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

    fn unseal_inner(self, key: &SealedKey, binding: Option<&[u8; 32]>) -> Result<[u8; 32], String> {
        let mut payload =
            self.0
                .unseal_object(&key.policy(), binding, &key.public, &key.private)?;
        let result = (|| {
            let mut payload = Reader(&payload);
            if payload.u32()? != key.uid {
                return Err("sealed TPM key belongs to another user".into());
            }
            let master = payload
                .take(32)?
                .try_into()
                .map_err(|_| "invalid unsealed key length")?;
            payload.end()?;
            Ok(master)
        })();
        payload.fill(0);
        result
    }
}

/// td-fido's CTAP assertion and enrollment codecs verify a signature
/// through this public-only TPM verification.
impl<T: Transport> super::fido_ctap::Es256Verifier for Client<T> {
    fn verify_es256(
        &mut self,
        x: &[u8; 32],
        y: &[u8; 32],
        digest: &[u8; 32],
        r: &[u8; 32],
        s: &[u8; 32],
    ) -> Result<(), String> {
        Client::verify_es256(self, x, y, digest, r, s)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::crypto;
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};
    use td_tpm::{
        CREATE, CREATE_PRIMARY, FLUSH_CONTEXT, LOAD, NO_SESSIONS, OWNER, PASSWORD, PCR_READ,
        POLICY_COMMAND_CODE, POLICY_GET_DIGEST, POLICY_PCR, SEALED_ATTRIBUTES, SESSIONS,
        START_AUTH_SESSION, UNSEAL,
    };

    const O_NOFOLLOW: i32 = 0o400000;

    fn random() -> [u8; 32] {
        let mut bytes = [0; 32];
        File::open("/dev/urandom")
            .unwrap()
            .read_exact(&mut bytes)
            .unwrap();
        bytes
    }

    // Disposable virtual-token signer, compiled only into the test harness.
    pub(crate) struct SigningKey {
        client: td_tpm::Client<Device>,
        handle: u32,
        pub(crate) cose: Vec<u8>,
    }

    impl SigningKey {
        pub(crate) fn new() -> Self {
            Self::in_hierarchy(NULL, &random())
        }

        pub(crate) fn persistent(unique: &[u8; 32]) -> Self {
            Self::in_hierarchy(OWNER, unique)
        }

        fn in_hierarchy(hierarchy: u32, unique: &[u8; 32]) -> Self {
            let mut client = td_tpm::Client::new(Device::open().unwrap());
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
                .call(
                    CREATE_PRIMARY,
                    &[hierarchy],
                    Some(PASSWORD),
                    &parameters,
                    true,
                )
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
                let first = value
                    .iter()
                    .position(|byte| *byte != 0)
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
            self.client().0.extend_pcr(7, digest).unwrap();
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

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// The envelopes and the policy digest are persisted formats (DESIGN.md);
    /// these literals move only with a deliberate format revision.
    #[test]
    fn sealed_and_bound_envelopes_keep_their_encoded_bytes() {
        let sealed = fixture(1000);
        let key = SealedKey::decode(&sealed).unwrap();
        let bound = BoundKey {
            binding: [9; 32],
            key: key.clone(),
        }
        .encode()
        .unwrap();
        assert_eq!(
            [
                hex(&crypto::digest(&sealed)),
                hex(&key.policy_digest()),
                hex(&crypto::digest(&bound)),
            ],
            [
                "44e78107e7fc6b3b04bbd9c6012ff4dd77e870c780e449ff316ef128d25e5115",
                // TPM2 PolicyPCR(SHA-256, PCR 7, [6; 32]) then
                // PolicyCommandCode(Unseal) from the zero digest.
                "ee2145ba1675359be7e16f37ac26258720226007456eed056b0b16ede976c3e9",
                "9c6c3f7fc953dd7e0e2b4c1d5531026af2bf366af8b080aecbf080b9087db7b5",
            ]
        );
    }

    /// A scripted TPM: canned, well-formed replies to the seal and unseal
    /// command profile, with PolicyPCR, PolicyCommandCode and the Unseal
    /// authPolicy check evaluated as the TPM does. Every command is recorded
    /// with its caller nonce zeroed, the client's only random field.
    struct ScriptedTpm {
        log: std::rc::Rc<std::cell::RefCell<Vec<Vec<u8>>>>,
        next: u32,
        sessions: Vec<(u32, [u8; 32])>,
        objects: Vec<(u32, Vec<u8>, Vec<u8>)>,
    }

    impl ScriptedTpm {
        fn new(log: &std::rc::Rc<std::cell::RefCell<Vec<Vec<u8>>>>) -> Self {
            Self {
                log: log.clone(),
                next: 0,
                sessions: Vec::new(),
                objects: Vec::new(),
            }
        }

        fn session(&mut self, handle: u32) -> &mut [u8; 32] {
            &mut self
                .sessions
                .iter_mut()
                .find(|(value, _)| *value == handle)
                .expect("unknown policy session")
                .1
        }

        fn creation(out: &mut Vec<u8>) {
            put_blob(out, &[]).unwrap();
            put_blob(out, &[0x0c; 32]).unwrap();
            put16(out, 0x8021);
            put32(out, OWNER);
            put_blob(out, &[0x0d; 32]).unwrap();
        }
    }

    impl Transport for ScriptedTpm {
        fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
            let code = u32::from_be_bytes(command[6..10].try_into().unwrap());
            let handles = match code {
                PCR_READ => 0,
                START_AUTH_SESSION => 2,
                _ => 1,
            };
            let mut recorded = command.to_vec();
            let mut at = 10 + 4 * handles;
            let handle =
                (handles == 1).then(|| u32::from_be_bytes(command[10..14].try_into().unwrap()));
            let mut session = None;
            if u16::from_be_bytes([command[0], command[1]]) == SESSIONS {
                let size = u32::from_be_bytes(command[at..at + 4].try_into().unwrap()) as usize;
                let auth = u32::from_be_bytes(command[at + 4..at + 8].try_into().unwrap());
                let nonce = usize::from(u16::from_be_bytes([command[at + 8], command[at + 9]]));
                if auth != PASSWORD {
                    assert_eq!(nonce, 32);
                    recorded[at + 10..at + 10 + nonce].fill(0);
                    session = Some(auth);
                }
                at += 4 + size;
            }
            let mut input = Reader(&command[at..]);
            if code == START_AUTH_SESSION {
                assert_eq!(&command[at..at + 2], [0, 32]);
                recorded[at + 2..at + 34].fill(0);
            }
            self.log.borrow_mut().push(recorded);
            self.next += 1;
            let mut out_handle = None;
            let mut out = Vec::new();
            match code {
                PCR_READ => {
                    assert_eq!(input.u32().unwrap(), 1);
                    assert_eq!(input.u16().unwrap(), SHA256);
                    assert_eq!(input.u8().unwrap(), 3);
                    let mask = input.take(3).unwrap();
                    input.end().unwrap();
                    put32(&mut out, 0x55);
                    put32(&mut out, 1);
                    put16(&mut out, SHA256);
                    out.push(3);
                    out.extend_from_slice(mask);
                    let bits = u32::from_le_bytes([mask[0], mask[1], mask[2], 0]);
                    put32(&mut out, bits.count_ones());
                    for index in (0..24).filter(|index| bits & (1 << index) != 0) {
                        put_blob(&mut out, &[index as u8 + 0x40; 32]).unwrap();
                    }
                }
                START_AUTH_SESSION => {
                    assert_eq!(input.blob().unwrap().len(), 32);
                    assert!(input.blob().unwrap().is_empty());
                    assert!(matches!(input.u8().unwrap(), 0x01 | 0x03));
                    assert_eq!(input.u16().unwrap(), ALG_NULL);
                    assert_eq!(input.u16().unwrap(), SHA256);
                    input.end().unwrap();
                    let handle = 0x0300_0000 + self.next;
                    self.sessions.push((handle, [0; 32]));
                    out_handle = Some(handle);
                    put_blob(&mut out, &[0x0e; 32]).unwrap();
                }
                POLICY_PCR => {
                    let digest = input.blob().unwrap().to_vec();
                    let mut selection = Vec::new();
                    put32(&mut selection, input.u32().unwrap());
                    put16(&mut selection, input.u16().unwrap());
                    selection.push(input.u8().unwrap());
                    selection.extend_from_slice(input.take(3).unwrap());
                    input.end().unwrap();
                    assert_eq!(selection[..7], [0, 0, 0, 1, 0, 0x0b, 3]);
                    let state = self.session(handle.unwrap());
                    let mut bytes = state.to_vec();
                    put32(&mut bytes, POLICY_PCR);
                    bytes.extend_from_slice(&selection);
                    bytes.extend_from_slice(&digest);
                    *state = crypto::digest(&bytes);
                }
                POLICY_COMMAND_CODE => {
                    let state = self.session(handle.unwrap());
                    let mut bytes = state.to_vec();
                    put32(&mut bytes, POLICY_COMMAND_CODE);
                    bytes.extend_from_slice(input.take(4).unwrap());
                    input.end().unwrap();
                    *state = crypto::digest(&bytes);
                }
                POLICY_GET_DIGEST => {
                    let state = *self.session(handle.unwrap());
                    put_blob(&mut out, &state).unwrap();
                }
                CREATE_PRIMARY => {
                    assert_eq!(handle, Some(OWNER));
                    assert_eq!(input.blob().unwrap(), [0; 4]);
                    let template = input.blob().unwrap();
                    // The fixed storage template precedes its unique field.
                    let mut public = template[..22].to_vec();
                    put_blob(&mut public, &crypto::digest(template)).unwrap();
                    put_blob(&mut public, &[0x0f; 32]).unwrap();
                    out_handle = Some(0x8000_0000 + self.next);
                    put_blob(&mut out, &public).unwrap();
                    Self::creation(&mut out);
                    let mut name = SHA256.to_be_bytes().to_vec();
                    name.extend_from_slice(&crypto::digest(&public));
                    put_blob(&mut out, &name).unwrap();
                }
                CREATE => {
                    let mut sensitive = Reader(input.blob().unwrap());
                    assert!(sensitive.blob().unwrap().is_empty());
                    let data = sensitive.blob().unwrap();
                    let template = input.blob().unwrap();
                    let mut public = template[..template.len() - 2].to_vec();
                    put_blob(&mut public, &crypto::digest(data)).unwrap();
                    let mut private = b"scripted:".to_vec();
                    private.extend_from_slice(data);
                    put_blob(&mut out, &private).unwrap();
                    put_blob(&mut out, &public).unwrap();
                    Self::creation(&mut out);
                }
                LOAD => {
                    let private = input.blob().unwrap();
                    let public = input.blob().unwrap();
                    input.end().unwrap();
                    let mut fields = Reader(public);
                    fields.take(8).unwrap();
                    let policy = fields.blob().unwrap().to_vec();
                    let handle = 0x8000_0000 + self.next;
                    let data = private.strip_prefix(b"scripted:").unwrap().to_vec();
                    self.objects.push((handle, policy, data));
                    out_handle = Some(handle);
                    let mut name = SHA256.to_be_bytes().to_vec();
                    name.extend_from_slice(&crypto::digest(public));
                    put_blob(&mut out, &name).unwrap();
                }
                UNSEAL => {
                    let session = session.unwrap();
                    let digest = *self.session(session);
                    self.sessions.retain(|(value, _)| *value != session);
                    let (_, policy, data) = self
                        .objects
                        .iter()
                        .find(|(value, ..)| Some(*value) == handle)
                        .unwrap();
                    assert_eq!(policy, &digest, "Unseal policy failure");
                    put_blob(&mut out, data).unwrap();
                }
                FLUSH_CONTEXT => {
                    let handle = handle.unwrap();
                    self.sessions.retain(|(value, _)| *value != handle);
                    self.objects.retain(|(value, ..)| *value != handle);
                }
                _ => panic!("unscripted TPM command {code:#x}"),
            }
            let tag = u16::from_be_bytes([command[0], command[1]]);
            let mut response = tag.to_be_bytes().to_vec();
            put32(&mut response, 0);
            put32(&mut response, 0);
            if let Some(handle) = out_handle {
                put32(&mut response, handle);
            }
            if tag == SESSIONS {
                put32(&mut response, out.len() as u32);
                response.extend_from_slice(&out);
                if session.is_some() {
                    put_blob(&mut response, &[0x0b; 32]).unwrap();
                    response.push(0);
                } else {
                    put_blob(&mut response, &[]).unwrap();
                    response.push(1);
                }
                put_blob(&mut response, &[]).unwrap();
            } else {
                response.extend_from_slice(&out);
            }
            let size = response.len() as u32;
            response[2..6].copy_from_slice(&size.to_be_bytes());
            Ok(response)
        }
    }

    /// The client's command stream is the other half of the persisted
    /// contract: what the TPM is asked to create, load and authorize. These
    /// literals move only with a deliberate protocol change.
    #[test]
    fn seal_and_unseal_send_their_recorded_command_stream() {
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let client = || Client::new(ScriptedTpm::new(&log));
        let mut phases = Vec::new();
        let mut mark = |log: &std::rc::Rc<std::cell::RefCell<Vec<Vec<u8>>>>| {
            let codes: Vec<u32> = log.borrow()[phases.iter().sum::<usize>()..]
                .iter()
                .map(|command| u32::from_be_bytes(command[6..10].try_into().unwrap()))
                .collect();
            phases.push(codes.len());
            codes
        };
        let sealed = client()
            .seal(1000, Pcrs::parse("7").unwrap(), &[0x21; 32])
            .unwrap();
        let seal = mark(&log);
        assert_eq!(client().unseal(&sealed).unwrap(), [0x21; 32]);
        let unseal = mark(&log);
        let binding = [0x5a; 32];
        let bound = client()
            .seal_bound(1001, Pcrs::parse("0,7,15").unwrap(), &[0x22; 32], &binding)
            .unwrap();
        let seal_bound = mark(&log);
        assert_eq!(client().unseal_bound(&bound, &binding).unwrap(), [0x22; 32]);
        let unseal_bound = mark(&log);
        assert_eq!(
            [seal, unseal, seal_bound, unseal_bound],
            [
                vec![
                    PCR_READ,
                    START_AUTH_SESSION,
                    POLICY_PCR,
                    POLICY_COMMAND_CODE,
                    POLICY_GET_DIGEST,
                    CREATE_PRIMARY,
                    CREATE,
                    FLUSH_CONTEXT,
                    FLUSH_CONTEXT,
                ],
                vec![
                    CREATE_PRIMARY,
                    LOAD,
                    START_AUTH_SESSION,
                    POLICY_PCR,
                    POLICY_COMMAND_CODE,
                    POLICY_GET_DIGEST,
                    UNSEAL,
                    FLUSH_CONTEXT,
                    FLUSH_CONTEXT,
                ],
                vec![
                    PCR_READ,
                    START_AUTH_SESSION,
                    POLICY_PCR,
                    POLICY_COMMAND_CODE,
                    POLICY_GET_DIGEST,
                    CREATE_PRIMARY,
                    FLUSH_CONTEXT,
                    CREATE_PRIMARY,
                    CREATE,
                    FLUSH_CONTEXT,
                    FLUSH_CONTEXT,
                ],
                vec![
                    CREATE_PRIMARY,
                    LOAD,
                    START_AUTH_SESSION,
                    POLICY_PCR,
                    POLICY_COMMAND_CODE,
                    POLICY_GET_DIGEST,
                    UNSEAL,
                    FLUSH_CONTEXT,
                    FLUSH_CONTEXT,
                ],
            ]
        );
        let mut at = 0;
        let per_phase: Vec<String> = phases
            .iter()
            .map(|count| {
                let commands = log.borrow()[at..at + count].concat();
                at += count;
                hex(&crypto::digest(&commands))
            })
            .collect();
        assert_eq!(
            per_phase,
            [
                "96d0325775f566d09fe058cafc60d14a37b613b24e51f667800e052834eb1759",
                "c6416d5eaafeeb3c238bd2c3e22b29bb7afc502a79f29a71d756b947df0aa932",
                "5ea50bcaa42f5506c327d2753e9d9549deb7920269c7226bf7a0ffa4ca31797b",
                "4fb3301ea764d1dd003d6c33d6b414b0a800701758b1de02fe0d49a0d776a5c5",
            ]
        );
        let stream = log.borrow().concat();
        assert_eq!(
            [
                hex(&crypto::digest(&stream)),
                hex(&crypto::digest(&sealed.encode().unwrap())),
                hex(&crypto::digest(&bound.encode().unwrap())),
            ],
            [
                "0579a58b4e1c69d60bff76270daa9ec467414e9736b89850a9d94737e3077c47",
                "438cfbef071ead971dc309d733c3b9f88995f20455d08187f99e0556b673b880",
                "eeae7fd20137180abc7c6a397feac122d8d309ff32dd6ac35833b4c15f515cbe",
            ]
        );
    }

    #[test]
    fn selections_and_malformed_envelopes_are_refused() {
        for value in ["", "16", "32", "0,0", "-1", "7,", "7 8", " 7"] {
            assert!(Pcrs::parse(value).is_err(), "{value}");
        }
        assert_eq!(Pcrs::parse("0,7,15").unwrap().0.mask(), 0x8081);
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
    fn snapshot_refuses_an_unmeasured_pcr_and_digests_the_values() {
        struct Reply(Vec<u8>);
        impl Transport for Reply {
            fn exchange(&mut self, _: &[u8]) -> Result<Vec<u8>, String> {
                Ok(self.0.clone())
            }
        }
        for value in [[4; 32], [0; 32]] {
            let mut parameters = Vec::new();
            put32(&mut parameters, 0);
            parameters.extend_from_slice(&PcrSelection::new(1 << 7).unwrap().marshal());
            put32(&mut parameters, 1);
            put_blob(&mut parameters, &value).unwrap();
            let mut response = NO_SESSIONS.to_be_bytes().to_vec();
            put32(&mut response, 10 + parameters.len() as u32);
            put32(&mut response, 0);
            response.extend_from_slice(&parameters);
            let snapshot = Client::new(Reply(response)).snapshot(Pcrs::parse("7").unwrap());
            if value == [0; 32] {
                assert_eq!(
                    snapshot.unwrap_err(),
                    "selected PCR is missing or unmeasured"
                );
            } else {
                assert_eq!(snapshot.unwrap(), crypto::digest(&value));
            }
        }
    }

    #[test]
    fn policy_downgrades_are_refused() {
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
            assert_eq!(
                client.0.owned_handles(),
                usize::from(matches!(fault, 7 | 10))
            );
            drop(client);
            assert_eq!(
                flushes.get(),
                if matches!(fault, 7 | 10) { 2 } else { loaded }
            );
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
            assert_eq!(client.0.owned_handles(), 0);
        }
        for field in 0..5 {
            let mut values = [x, y, digest, r, s];
            values[field][0] ^= 1;
            let [x, y, digest, r, s] = values;
            assert!(
                client.verify_es256(&x, &y, &digest, &r, &s).is_err(),
                "field {field}"
            );
            assert_eq!(client.0.owned_handles(), 0);
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
            .0
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
        assert!(
            std::fs::read_to_string("/proc/cmdline")
                .unwrap()
                .split_whitespace()
                .any(|word| word == "td.tpm-fixture=1"),
            "requires the explicitly selected disposable TPM guest"
        );
        assert_eq!(std::fs::read_to_string("/case").unwrap(), case);
    }

    fn qemu_client() -> Client<Device> {
        Client::new(Device::open().unwrap())
    }

    pub(crate) fn qemu_boot_pcr_digest() -> [u8; 32] {
        qemu_client().snapshot(Pcrs::parse("11").unwrap()).unwrap()
    }

    pub(crate) fn qemu_extend(digest: &[u8; 32]) {
        qemu_client().0.extend_pcr(7, digest).unwrap();
    }

    fn qemu_disk(write: bool) -> File {
        let file = OpenOptions::new()
            .read(true)
            .write(write)
            .custom_flags(O_NOFOLLOW)
            .open("/dev/vda")
            .unwrap();
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
        assert_eq!(
            qemu_client().snapshot(Pcrs::parse("7").unwrap()).unwrap(),
            key.key.pcr_digest,
            "cold boot did not reproduce the fixture PCR state"
        );
        key
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable TPM and disk"]
    fn qemu_device_seals_to_persistent_state() {
        use std::io::{Seek, SeekFrom};
        qemu_guard("tpm-seal");
        qemu_extend(&[9; 32]);
        let sealed = qemu_client()
            .seal_bound(1000, Pcrs::parse("7").unwrap(), &QEMU_KEY, &QEMU_BINDING)
            .unwrap();
        assert_eq!(
            qemu_client().unseal_bound(&sealed, &QEMU_BINDING).unwrap(),
            QEMU_KEY
        );
        let encoded = sealed.encode().unwrap();
        let mut disk = qemu_disk(true);
        let mut empty = [0; 4096];
        disk.read_exact(&mut empty).unwrap();
        assert!(
            empty.iter().all(|byte| *byte == 0),
            "fixture disk is not fresh"
        );
        disk.seek(SeekFrom::Start(0)).unwrap();
        disk.write_all(QEMU_DISK_MAGIC).unwrap();
        disk.write_all(&u32::try_from(encoded.len()).unwrap().to_be_bytes())
            .unwrap();
        disk.write_all(&encoded).unwrap();
        disk.sync_all().unwrap();
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable TPM and disk"]
    fn qemu_device_reopens_after_cold_boot() {
        qemu_guard("tpm-reopen");
        let sealed = qemu_read_key();
        assert_eq!(
            qemu_client().unseal_bound(&sealed, &QEMU_BINDING).unwrap(),
            QEMU_KEY
        );
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable TPM and disk"]
    fn qemu_device_refuses_changed_pcr() {
        qemu_guard("tpm-pcr");
        let sealed = qemu_read_key();
        assert_eq!(
            qemu_client().unseal_bound(&sealed, &QEMU_BINDING).unwrap(),
            QEMU_KEY
        );
        qemu_extend(&[8; 32]);
        assert_ne!(
            qemu_client().snapshot(Pcrs::parse("7").unwrap()).unwrap(),
            sealed.key.pcr_digest
        );
        let error = qemu_client()
            .unseal_bound(&sealed, &QEMU_BINDING)
            .unwrap_err();
        assert!(error.starts_with("TPM command 0x17f refused:"), "{error}");
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable TPM and disk"]
    fn qemu_device_refuses_another_tpm() {
        qemu_guard("tpm-other");
        let sealed = qemu_read_key();
        let error = qemu_client()
            .unseal_bound(&sealed, &QEMU_BINDING)
            .unwrap_err();
        assert!(error.starts_with("TPM command 0x157 refused:"), "{error}");
    }
}

/// td-fido's codecs over this TPM verification, on the swtpm emulator:
/// the independent OpenSSL assertion fixture both tests sign with.
#[cfg(test)]
mod assertions {
    use super::super::fido_cbor::{self as cbor, Encoder, Value};
    use super::super::fido_ctap::{AssertionRequest, Es256PublicKey, RP_ID};
    use super::super::fido_enroll::{Info, MakeCredential};
    use super::tests::Emulator;
    use crate::crypto;

    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    fn cose() -> Vec<u8> {
        hex(concat!(
            "a5010203262001215820",
            "ab8ace3ba858575dd060bf6e790f73982165b36abbfffb86cf0f5e032fafbb5a",
            "225820552ef0c808cfa668e3012f4411fc0a3ad01a39d3a0fb158534721a8016b31553"
        ))
    }

    #[test]
    #[ignore = "requires explicitly supplied pinned host swtpm; never accesses hardware"]
    fn emulator_assertion_binds_challenge_extensions_and_signature() {
        // Independently signed using host OpenSSL 3.5.7; no private key or runtime dependency.
        let key = Es256PublicKey::from_cose(&cose()).unwrap();
        let client = hex("f3ad24f2731ea324507944e3ae1b9a172f14eaac6a57e004788390dc14a4c7ca");
        let auth = hex("34e2ef54cd9003d2930734cfb0402ccab6a44dcb5024fc367878c413c78ce2dd8100000007a16b6372656450726f7465637401");
        let signature = hex("3045022100fcd359f2e59ed2e63367ec882724beae6d78fd876d9208b9ec0900b4114aa98c02205c58e5c6d917e85e879fed77b43f0b73cf4394eb1caadb657855e362e541b4f7");
        let client: [u8; 32] = client.try_into().unwrap();
        let root = std::env::temp_dir().join(format!("td-ctap-oracle-{}", std::process::id()));
        assert!(!root.exists());
        let emulator = Emulator::start(&root);
        let mut tpm = emulator.client();
        let response = |auth: &[u8], sig: &[u8]| {
            let mut out = Encoder::new();
            out.head(5, 2).unwrap();
            out.head(0, 2).unwrap();
            out.bytes(auth).unwrap();
            out.head(0, 3).unwrap();
            out.bytes(sig).unwrap();
            let mut bytes = vec![0];
            bytes.extend(out.finish().unwrap());
            bytes
        };
        let good = response(&auth, &signature);
        let info = AssertionRequest::new(&[7], client, 1024)
            .unwrap()
            .verify(&good, &key, &mut tpm)
            .unwrap();
        assert_eq!(info.counter, 7);
        let mut changed_client = client;
        changed_client[0] ^= 1;
        assert!(AssertionRequest::new(&[7], changed_client, 1024)
            .unwrap()
            .verify(&good, &key, &mut tpm)
            .is_err());
        let mut changed_extension = auth.clone();
        *changed_extension.last_mut().unwrap() = 2;
        let changed = response(&changed_extension, &signature);
        assert!(AssertionRequest::new(&[7], client, 1024)
            .unwrap()
            .verify(&changed, &key, &mut tpm)
            .is_err());
        let mut changed_signature = signature;
        *changed_signature.last_mut().unwrap() ^= 1;
        let changed = response(&auth, &changed_signature);
        assert!(AssertionRequest::new(&[7], client, 1024)
            .unwrap()
            .verify(&changed, &key, &mut tpm)
            .is_err());
        drop(tpm);
        drop(emulator);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn standard() -> Info {
        let mut out = Encoder::new();
        out.head(5, 4).unwrap();
        out.head(0, 1).unwrap();
        out.head(4, 1).unwrap();
        out.text("FIDO_2_1").unwrap();
        out.head(0, 3).unwrap();
        out.bytes(&[0; 16]).unwrap();
        out.head(0, 4).unwrap();
        out.head(5, 0).unwrap();
        out.head(0, 5).unwrap();
        out.head(0, 1024).unwrap();
        let mut bytes = vec![0];
        bytes.extend(out.finish().unwrap());
        Info::parse(&bytes).unwrap()
    }

    fn auth() -> Vec<u8> {
        let mut data = crypto::digest(RP_ID.as_bytes()).to_vec();
        data.push(0x41);
        data.extend_from_slice(&[0; 4]);
        data.extend_from_slice(&[0; 16]);
        data.extend_from_slice(&[0, 1, 7]);
        data.extend(cose());
        data
    }

    fn response(data: &[u8]) -> Vec<u8> {
        let mut out = Encoder::new();
        out.head(5, 3).unwrap();
        out.head(0, 1).unwrap();
        out.text("none").unwrap();
        out.head(0, 2).unwrap();
        out.bytes(data).unwrap();
        out.head(0, 3).unwrap();
        out.head(5, 0).unwrap();
        let mut bytes = vec![0];
        bytes.extend(out.finish().unwrap());
        bytes
    }

    fn request() -> MakeCredential {
        MakeCredential::primary(standard(), [1; 32], [2; 32]).unwrap()
    }

    #[test]
    #[ignore = "requires explicitly supplied pinned host swtpm; never accesses hardware"]
    fn emulator_enrollment_requires_fresh_proof_under_the_created_key() {
        struct Directory(std::path::PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = std::env::temp_dir().join(format!(
            "td-enrollment-oracle-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(!root.exists());
        let _directory = Directory(root.clone());
        let emulator = Emulator::start(&root);
        let mut tpm = emulator.client();
        let client: [u8; 32] =
            hex("f3ad24f2731ea324507944e3ae1b9a172f14eaac6a57e004788390dc14a4c7ca")
                .try_into()
                .unwrap();
        let auth_data = hex("34e2ef54cd9003d2930734cfb0402ccab6a44dcb5024fc367878c413c78ce2dd8100000007a16b6372656450726f7465637401");
        let signature = hex("3045022100fcd359f2e59ed2e63367ec882724beae6d78fd876d9208b9ec0900b4114aa98c02205c58e5c6d917e85e879fed77b43f0b73cf4394eb1caadb657855e362e541b4f7");
        let mut out = Encoder::new();
        out.head(5, 2).unwrap();
        out.head(0, 2).unwrap();
        out.bytes(&auth_data).unwrap();
        out.head(0, 3).unwrap();
        out.bytes(&signature).unwrap();
        let mut signed = vec![0];
        signed.extend(out.finish().unwrap());
        let made = response(&auth());
        let proof = MakeCredential::primary(standard(), [1; 32], [2; 32])
            .unwrap()
            .proof(&made, client)
            .unwrap();
        assert_eq!(proof.bytes()[0], 2);
        let enrolled = proof.verify(&signed, &mut tpm).unwrap();
        assert_eq!(enrolled.id(), [7]);
        assert_eq!(enrolled.cose(), cose());
        let recovery = MakeCredential::recovery(standard(), [1; 32], [2; 32], &enrolled).unwrap();
        let wire = cbor::decode(&recovery.bytes()[1..]).unwrap();
        let Value::Array(exclude) = wire.required(&Value::Unsigned(5)).unwrap() else {
            panic!("exclude list is not an array");
        };
        assert_eq!(
            exclude[0]
                .required(&Value::Text("id"))
                .unwrap()
                .bytes()
                .unwrap(),
            enrolled.id()
        );
        assert!(recovery.proof(&made, client).is_err());
        let mut wrong_client = client;
        wrong_client[0] ^= 1;
        assert!(request()
            .proof(&made, wrong_client)
            .unwrap()
            .verify(&signed, &mut tpm)
            .is_err());
        let mut wrong_key = auth();
        *wrong_key.last_mut().unwrap() ^= 1;
        assert!(request()
            .proof(&response(&wrong_key), client)
            .unwrap()
            .verify(&signed, &mut tpm)
            .is_err());
        *signed.last_mut().unwrap() ^= 1;
        assert!(request()
            .proof(&made, client)
            .unwrap()
            .verify(&signed, &mut tpm)
            .is_err());
    }
}
