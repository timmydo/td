//! Login record codec, verifier and client-data hash (td-login/TOKEN-LOGIN.md).
//! Pure: no file, token or entropy access. Records are public metadata.

use super::crypto;
use super::fido_ctap::{self, MAX_CREDENTIAL_ID};
use super::fido_p256::PublicKey;

type Result<T> = std::result::Result<T, String>;

pub(super) const MAGIC: &[u8; 8] = b"TDLOGREC";
pub(super) const VERSION: u8 = 1;
/// Record versions this build reads; a tier marker lists exactly these.
pub(super) const READS: &[u8] = &[VERSION];
pub(super) const MAX_SLOTS: usize = 8;
const KEY_BYTES: usize = 64;
const HEADER: usize = MAGIC.len() + 1 + 4 + 32 + 1;
const MAX_SLOT: usize = 2 + MAX_CREDENTIAL_ID + KEY_BYTES + 32 + 32;
pub(super) const MAX_RECORD: usize = HEADER + MAX_SLOTS * MAX_SLOT;
const VERIFIER: &[u8] = b"td-login/verifier/v1\0";
const OPERATION: &[u8] = b"td-login/operation/v1\0";

/// A proved key and its PIN-verified hmac-secret output, which the caller
/// still owns and clears. No Debug: it borrows that secret.
pub(super) struct NewKey<'a> {
    pub credential: Vec<u8>,
    pub key: PublicKey,
    pub salt: [u8; 32],
    pub output: &'a [u8; 32],
}

/// One enrolled key. No Debug or Clone: the key type has neither. Only a
/// `Record` builds one, so its verifier binds that record's UID and ID.
pub(super) struct Slot {
    credential: Vec<u8>,
    key: PublicKey,
    salt: [u8; 32],
    verifier: [u8; 32],
}

impl Slot {
    fn derive(uid: u32, id: &[u8; 32], new: NewKey<'_>) -> Result<Self> {
        let derived = verifier(uid, id, &new.credential, new.output)?;
        Ok(Self {
            credential: new.credential,
            key: new.key,
            salt: new.salt,
            verifier: derived.0,
        })
    }

    pub fn credential(&self) -> &[u8] {
        &self.credential
    }

    pub fn key(&self) -> &PublicKey {
        &self.key
    }

    pub fn salt(&self) -> &[u8; 32] {
        &self.salt
    }

    pub fn fingerprint(&self) -> [u8; 4] {
        fido_ctap::fingerprint(&self.credential)
    }
}

/// A decoded or proposed record: valid by construction.
pub(super) struct Record {
    uid: u32,
    id: [u8; 32],
    version: u8,
    slots: Vec<Slot>,
    digest: [u8; 32],
}

impl Record {
    /// First enrollment: every verifier binds this record's UID and ID.
    pub fn enroll(uid: u32, id: [u8; 32], version: u8, keys: Vec<NewKey<'_>>) -> Result<Self> {
        if !(1..=MAX_SLOTS).contains(&keys.len()) {
            return Err("login record requires one through eight keys".into());
        }
        let slots = keys
            .into_iter()
            .map(|key| Slot::derive(uid, &id, key))
            .collect::<Result<Vec<_>>>()?;
        Self::assemble(uid, id, version, slots)
    }

    /// Adds one proved key, keeping the UID, ID and every enrolled slot.
    pub fn with_key(self, version: u8, key: NewKey<'_>) -> Result<Self> {
        let Self {
            uid, id, mut slots, ..
        } = self;
        if slots.len() >= MAX_SLOTS {
            return Err("login record already holds eight keys".into());
        }
        if slots.iter().any(|slot| slot.credential == key.credential) {
            return Err("login credential is already enrolled".into());
        }
        slots.push(Slot::derive(uid, &id, key)?);
        Self::assemble(uid, id, version, slots)
    }

    /// Removes a nonempty set of enrolled keys, keeping the UID and ID.
    /// Removing every key refuses: the store unlinks the record instead.
    pub fn without(self, version: u8, credentials: &[&[u8]]) -> Result<Self> {
        if credentials.is_empty() {
            return Err("no login key selected for removal".into());
        }
        for (index, credential) in credentials.iter().enumerate() {
            if self.slot(credential).is_none() {
                return Err("login credential is not enrolled".into());
            }
            if credentials.iter().take(index).any(|c| c == credential) {
                return Err("login key selected twice for removal".into());
            }
        }
        let Self { uid, id, slots, .. } = self;
        let kept: Vec<Slot> = slots
            .into_iter()
            .filter(|slot| !credentials.contains(&slot.credential.as_slice()))
            .collect();
        if kept.is_empty() {
            return Err("removing every login key unlinks the record instead".into());
        }
        Self::assemble(uid, id, version, kept)
    }

    fn assemble(uid: u32, id: [u8; 32], version: u8, mut slots: Vec<Slot>) -> Result<Self> {
        if !READS.contains(&version) {
            return Err("unknown login record version".into());
        }
        slots.sort_by(|a, b| a.credential.cmp(&b.credential));
        validate(&slots)?;
        let mut record = Self {
            uid,
            id,
            version,
            slots,
            digest: [0; 32],
        };
        record.digest = crypto::digest(&record.encode()?);
        Ok(record)
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }

    #[cfg(test)]
    pub fn id(&self) -> &[u8; 32] {
        &self.id
    }

    pub fn version(&self) -> u8 {
        self.version
    }

    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    pub fn slot(&self, credential: &[u8]) -> Option<&Slot> {
        self.slots.iter().find(|slot| slot.credential == credential)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        validate(&self.slots)?;
        let mut out = Vec::with_capacity(MAX_RECORD);
        out.extend_from_slice(MAGIC);
        out.push(self.version);
        out.extend_from_slice(&self.uid.to_be_bytes());
        out.extend_from_slice(&self.id);
        out.push(u8::try_from(self.slots.len()).map_err(|_| "login key count overflow")?);
        for slot in &self.slots {
            let size = u16::try_from(slot.credential.len())
                .map_err(|_| "login credential length overflow")?;
            out.extend_from_slice(&size.to_be_bytes());
            out.extend_from_slice(&slot.credential);
            let (x, y) = slot.key.coordinates();
            out.extend_from_slice(&x);
            out.extend_from_slice(&y);
            out.extend_from_slice(&slot.salt);
            out.extend_from_slice(&slot.verifier);
        }
        Ok(out)
    }

    /// SHA-256 of the exact bytes decoded, or of a built record's encoding.
    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }

    pub fn decode(bytes: &[u8], expected_uid: u32) -> Result<Self> {
        if bytes.len() > MAX_RECORD {
            return Err("login record is oversized".into());
        }
        let mut reader = Reader(bytes);
        if reader.array::<8>()? != *MAGIC {
            return Err("not a login record".into());
        }
        let [version] = reader.array()?;
        if !READS.contains(&version) {
            return Err("unknown login record version".into());
        }
        let uid = u32::from_be_bytes(reader.array()?);
        if uid != expected_uid {
            return Err("login record is for another UID".into());
        }
        let id = reader.array()?;
        let [count] = reader.array()?;
        if !(1..=MAX_SLOTS).contains(&usize::from(count)) {
            return Err("login record requires one through eight keys".into());
        }
        let mut slots = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let size = usize::from(u16::from_be_bytes(reader.array()?));
            if size == 0 || size > MAX_CREDENTIAL_ID {
                return Err("invalid login credential length".into());
            }
            let credential = reader.take(size)?.to_vec();
            let x = reader.array()?;
            let y = reader.array()?;
            slots.push(Slot {
                credential,
                key: PublicKey::from_coordinates(&x, &y)
                    .map_err(|error| format!("login record key: {error}"))?,
                salt: reader.array()?,
                verifier: reader.array()?,
            });
        }
        reader.end()?;
        validate(&slots)?;
        Ok(Self {
            uid,
            id,
            version,
            slots,
            digest: crypto::digest(bytes),
        })
    }

    /// Whether `output` reproduces the slot's verifier, in constant time.
    pub fn check(&self, credential: &[u8], output: &[u8; 32]) -> Result<bool> {
        let slot = self
            .slot(credential)
            .ok_or("login credential is not enrolled")?;
        let derived = verifier(self.uid, &self.id, credential, output)?;
        Ok(check(&slot.verifier, &derived))
    }
}

fn validate(slots: &[Slot]) -> Result<()> {
    if !(1..=MAX_SLOTS).contains(&slots.len()) {
        return Err("login record requires one through eight keys".into());
    }
    let mut previous: Option<&[u8]> = None;
    for slot in slots {
        let credential = slot.credential.as_slice();
        if credential.is_empty() || credential.len() > MAX_CREDENTIAL_ID {
            return Err("invalid login credential length".into());
        }
        if previous.is_some_and(|p| p >= credential) {
            return Err("duplicate or unordered login credentials".into());
        }
        previous = Some(credential);
    }
    Ok(())
}

/// The lowest version this build and both retained deployments read.
pub(super) fn write_version(current: &[u8], previous: &[u8]) -> Result<u8> {
    lowest_common(READS, current, previous)
        .ok_or_else(|| "no login record version both deployments read".into())
}

fn lowest_common(own: &[u8], current: &[u8], previous: &[u8]) -> Option<u8> {
    own.iter()
        .copied()
        .filter(|version| current.contains(version) && previous.contains(version))
        .min()
}

/// A derived verifier. No Debug or Clone; cleared on drop.
struct Verifier([u8; 32]);

impl Drop for Verifier {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&mut self.0);
    }
}

/// HKDF-SHA256 with an empty salt; the info binds the label, UID, record
/// ID and length-prefixed credential ID.
fn verifier(uid: u32, id: &[u8; 32], credential: &[u8], output: &[u8; 32]) -> Result<Verifier> {
    if credential.is_empty() || credential.len() > MAX_CREDENTIAL_ID {
        return Err("invalid login credential length".into());
    }
    let mut info = Vec::with_capacity(VERIFIER.len() + 4 + 32 + 4 + credential.len());
    info.extend_from_slice(VERIFIER);
    info.extend_from_slice(&uid.to_be_bytes());
    info.extend_from_slice(id);
    put_field(&mut info, credential)?;
    Ok(Verifier(crypto::hkdf(output, &[], &info)))
}

fn check(stored: &[u8; 32], derived: &Verifier) -> bool {
    td_tpm::equal(stored, &derived.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Identify = 1,
    Authorize = 2,
    Create = 3,
    Prove = 4,
    Repeat = 5,
    Probe = 6,
    Unlock = 7,
    // 8 is reserved: consent's connect step sends no assertion.
}

/// `baseline` is the record an authorize phase changes; no other phase has one.
pub(super) fn client_data_hash(
    phase: Phase,
    description: &[u8],
    baseline: Option<&Record>,
    random: &[u8; 32],
) -> Result<[u8; 32]> {
    let mut input = OPERATION.to_vec();
    input.push(phase as u8);
    put_field(&mut input, description)?;
    match (phase, baseline) {
        (Phase::Authorize, Some(record)) => {
            put_field(&mut input, &record.id)?;
            put_field(&mut input, &record.digest)?;
        }
        (Phase::Authorize, None) => return Err("authorization requires its record".into()),
        (_, Some(_)) => return Err("only authorization binds a record".into()),
        (_, None) => {}
    }
    input.extend_from_slice(random);
    Ok(crypto::digest(&input))
}

fn put_field(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    let size = u32::try_from(bytes.len()).map_err(|_| "login field length overflow")?;
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, size: usize) -> Result<&'a [u8]> {
        let (bytes, rest) = self
            .0
            .split_at_checked(size)
            .ok_or("truncated login record")?;
        self.0 = rest;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?
            .try_into()
            .map_err(|_| "invalid login record field extent".into())
    }

    fn end(self) -> Result<()> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err("trailing login record bytes".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VECTORS: &str = include_str!("../tests/login_record_vectors.txt");
    const UID: u32 = 1000;

    fn vector(name: &str) -> Vec<u8> {
        let hex = VECTORS
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))
            .unwrap_or_else(|| panic!("missing vector {name}"));
        hex.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    fn array<const N: usize>(name: &str) -> [u8; N] {
        vector(name).try_into().unwrap()
    }

    fn key(index: usize) -> PublicKey {
        PublicKey::from_coordinates(
            &array(&format!("slot{index}_x")),
            &array(&format!("slot{index}_y")),
        )
        .unwrap()
    }

    fn output(index: usize) -> [u8; 32] {
        array(&format!("slot{index}_output"))
    }

    fn new_key(index: usize, output: &[u8; 32]) -> NewKey<'_> {
        NewKey {
            credential: vector(&format!("slot{index}_credential")),
            key: key(index),
            salt: array(&format!("slot{index}_salt")),
            output,
        }
    }

    fn synthetic<'a>(credential: &[u8], output: &'a [u8; 32]) -> NewKey<'a> {
        NewKey {
            credential: credential.to_vec(),
            key: key(0),
            salt: [0; 32],
            output,
        }
    }

    fn enroll(id: [u8; 32], indices: &[usize]) -> Result<Record> {
        let outputs: Vec<_> = indices.iter().map(|index| output(*index)).collect();
        let keys = indices
            .iter()
            .zip(&outputs)
            .map(|(index, output)| new_key(*index, output))
            .collect();
        Record::enroll(UID, id, VERSION, keys)
    }

    fn fixture() -> Record {
        enroll(array("id"), &[0, 1, 2]).unwrap()
    }

    fn slot_bytes(credential: &[u8], index: usize) -> Vec<u8> {
        let mut out = u16::try_from(credential.len())
            .unwrap()
            .to_be_bytes()
            .to_vec();
        out.extend_from_slice(credential);
        out.extend(vector(&format!("slot{index}_x")));
        out.extend(vector(&format!("slot{index}_y")));
        out.extend([0x5a; 64]);
        out
    }

    fn record_bytes(count: u8, slots: &[Vec<u8>]) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.push(VERSION);
        out.extend(UID.to_be_bytes());
        out.extend(vector("id"));
        out.push(count);
        for slot in slots {
            out.extend(slot);
        }
        out
    }

    fn refusal(bytes: &[u8]) -> String {
        Record::decode(bytes, UID)
            .err()
            .expect("decode should refuse")
    }

    #[test]
    fn record_and_verifiers_match_independent_vectors() {
        assert_eq!(vector("uid"), UID.to_be_bytes());
        let record = fixture();
        let bytes = record.encode().unwrap();
        assert_eq!(bytes, vector("record"));
        assert_eq!(record.digest().to_vec(), vector("digest"));
        assert_eq!(crypto::digest(&bytes).to_vec(), vector("digest"));
        let decoded = Record::decode(&bytes, UID).unwrap();
        assert_eq!(decoded.encode().unwrap(), bytes);
        assert_eq!(decoded.digest().to_vec(), vector("digest"));
        assert_eq!(decoded.digest(), crypto::digest(&decoded.encode().unwrap()));
        assert_eq!(
            (decoded.uid(), decoded.id(), decoded.version()),
            (UID, &array("id"), VERSION)
        );
        let order: Vec<&[u8]> = decoded.slots().iter().map(Slot::credential).collect();
        assert_eq!(
            order,
            [&[1u8; 64][..], b"A", b"login-fixture-credential-backup"]
        );
        for index in 0..3 {
            let credential = vector(&format!("slot{index}_credential"));
            let output = array(&format!("slot{index}_output"));
            let derived = verifier(UID, &array("id"), &credential, &output).unwrap();
            assert_eq!(derived.0.to_vec(), vector(&format!("slot{index}_verifier")));
            let slot = decoded.slot(&credential).unwrap();
            assert_eq!(
                slot.verifier.to_vec(),
                vector(&format!("slot{index}_verifier"))
            );
            assert_eq!(
                slot.fingerprint().to_vec(),
                vector(&format!("slot{index}_fingerprint"))
            );
            assert_eq!(slot.salt().to_vec(), vector(&format!("slot{index}_salt")));
            assert_eq!(
                slot.key().coordinates(),
                (
                    array(&format!("slot{index}_x")),
                    array(&format!("slot{index}_y"))
                )
            );
            assert!(decoded.check(&credential, &output).unwrap());
            for byte in 0..32 {
                let mut wrong = output;
                wrong[byte] ^= 1;
                assert!(!decoded.check(&credential, &wrong).unwrap());
            }
        }
        assert!(decoded.check(b"not enrolled", &[0; 32]).is_err());
    }

    #[test]
    fn the_verifier_binds_uid_record_and_credential() {
        let id = array("id");
        let output = array("slot2_output");
        let base = verifier(UID, &id, b"A", &output).unwrap();
        let mut other_id = id;
        other_id[0] ^= 1;
        for other in [
            verifier(UID + 1, &id, b"A", &output),
            verifier(UID, &other_id, b"A", &output),
            verifier(UID, &id, b"B", &output),
            verifier(UID, &id, b"AA", &output),
        ] {
            assert!(!check(&base.0, &other.unwrap()));
        }
        assert!(check(&base.0, &verifier(UID, &id, b"A", &output).unwrap()));
        assert!(verifier(UID, &id, b"", &output).is_err());
        assert!(verifier(UID, &id, &[0; MAX_CREDENTIAL_ID + 1], &output).is_err());
    }

    #[test]
    fn client_data_hashes_match_independent_vectors() {
        let record = fixture();
        let description = vector("description");
        let random = array("random");
        for (name, phase) in [
            ("identify", Phase::Identify),
            ("authorize", Phase::Authorize),
            ("create", Phase::Create),
            ("prove", Phase::Prove),
            ("repeat", Phase::Repeat),
            ("probe", Phase::Probe),
            ("unlock", Phase::Unlock),
        ] {
            let baseline = (phase == Phase::Authorize).then_some(&record);
            assert_eq!(
                client_data_hash(phase, &description, baseline, &random)
                    .unwrap()
                    .to_vec(),
                vector(&format!("hash_{name}")),
                "{name}"
            );
        }
    }

    #[test]
    fn authorize_alone_binds_the_exact_record() {
        let record = fixture();
        let random = array("random");
        assert!(client_data_hash(Phase::Authorize, b"d", None, &random).is_err());
        for phase in [
            Phase::Identify,
            Phase::Create,
            Phase::Prove,
            Phase::Repeat,
            Phase::Probe,
            Phase::Unlock,
        ] {
            assert!(client_data_hash(phase, b"d", Some(&record), &random).is_err());
        }
        // Dropping one key changes the digest and so the authorization.
        let smaller = enroll(array("id"), &[0, 1]).unwrap();
        assert_ne!(
            client_data_hash(Phase::Authorize, b"d", Some(&record), &random).unwrap(),
            client_data_hash(Phase::Authorize, b"d", Some(&smaller), &random).unwrap()
        );
    }

    #[test]
    fn every_truncation_and_trailing_byte_refuses() {
        let bytes = vector("record");
        for length in 0..bytes.len() {
            assert!(Record::decode(&bytes[..length], UID).is_err(), "{length}");
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(refusal(&trailing), "trailing login record bytes");
    }

    #[test]
    fn header_refusals() {
        let bytes = vector("record");
        let mut magic = bytes.clone();
        magic[0] ^= 1;
        assert_eq!(refusal(&magic), "not a login record");
        for version in [0u8, 2, 0xff] {
            let mut unknown = bytes.clone();
            unknown[8] = version;
            assert_eq!(refusal(&unknown), "unknown login record version");
        }
        assert_eq!(
            Record::decode(&bytes, UID + 1).err().unwrap(),
            "login record is for another UID"
        );
        assert_eq!(
            Record::decode(&bytes, 0).err().unwrap(),
            "login record is for another UID"
        );
    }

    #[test]
    fn slot_count_order_and_credential_refusals() {
        let one = slot_bytes(b"A", 0);
        assert!(Record::decode(&record_bytes(1, std::slice::from_ref(&one)), UID).is_ok());
        assert_eq!(
            refusal(&record_bytes(0, &[])),
            "login record requires one through eight keys"
        );
        let nine: Vec<_> = (0u8..9).map(|i| slot_bytes(&[i + 1], 0)).collect();
        assert_eq!(
            refusal(&record_bytes(9, &nine)),
            "login record requires one through eight keys"
        );
        let eight = &nine[..8];
        assert!(Record::decode(&record_bytes(8, eight), UID).is_ok());
        assert_eq!(
            refusal(&record_bytes(2, &[one.clone(), one.clone()])),
            "duplicate or unordered login credentials"
        );
        let later = slot_bytes(b"B", 1);
        assert!(Record::decode(&record_bytes(2, &[one.clone(), later.clone()]), UID).is_ok());
        assert_eq!(
            refusal(&record_bytes(2, &[later, one])),
            "duplicate or unordered login credentials"
        );
        assert_eq!(
            refusal(&record_bytes(1, &[slot_bytes(b"", 0)])),
            "invalid login credential length"
        );
        let largest = vec![7; MAX_CREDENTIAL_ID];
        assert!(Record::decode(&record_bytes(1, &[slot_bytes(&largest, 0)]), UID).is_ok());
        assert_eq!(
            refusal(&record_bytes(
                1,
                &[slot_bytes(&[7; MAX_CREDENTIAL_ID + 1], 0)]
            )),
            "invalid login credential length"
        );
    }

    #[test]
    fn off_curve_and_noncanonical_keys_refuse() {
        let mut off_curve = slot_bytes(b"A", 0);
        off_curve[2 + 1 + 63] ^= 1;
        assert_eq!(
            refusal(&record_bytes(1, &[off_curve])),
            "login record key: P-256 public key is not on the curve"
        );
        let mut overwide = slot_bytes(b"A", 0);
        overwide[3..35].fill(0xff);
        assert_eq!(
            refusal(&record_bytes(1, &[overwide])),
            "login record key: noncanonical P-256 integer"
        );
    }

    #[test]
    fn the_largest_record_is_the_bound() {
        let slots: Vec<_> = (0u8..8)
            .map(|i| slot_bytes(&[i; MAX_CREDENTIAL_ID], usize::from(i % 3)))
            .collect();
        let mut bytes = record_bytes(8, &slots);
        assert_eq!(bytes.len(), MAX_RECORD);
        assert_eq!(MAX_RECORD, 9278);
        let record = Record::decode(&bytes, UID).unwrap();
        assert_eq!(record.encode().unwrap(), bytes);
        bytes.push(0);
        assert_eq!(refusal(&bytes), "login record is oversized");
    }

    #[test]
    fn enrollment_sorts_and_refuses_what_decode_refuses() {
        let id = array("id");
        let zero = [0; 32];
        assert!(Record::enroll(UID, id, VERSION, Vec::new()).is_err());
        assert!(Record::enroll(UID, id, 2, vec![synthetic(b"A", &zero)]).is_err());
        let twice = vec![synthetic(b"A", &zero), synthetic(b"A", &zero)];
        assert!(Record::enroll(UID, id, VERSION, twice).is_err());
        let credentials: Vec<[u8; 1]> = (1u8..=9).map(|i| [i]).collect();
        let nine = credentials.iter().map(|c| synthetic(c, &zero)).collect();
        assert!(Record::enroll(UID, id, VERSION, nine).is_err());
        assert!(Record::enroll(UID, id, VERSION, vec![synthetic(b"", &zero)]).is_err());
        let long = [7; MAX_CREDENTIAL_ID + 1];
        assert!(Record::enroll(UID, id, VERSION, vec![synthetic(&long, &zero)]).is_err());
        let record = enroll(id, &[2, 1, 0]).unwrap();
        assert_eq!(record.encode().unwrap(), vector("record"));
    }

    #[test]
    fn verifiers_bind_the_record_they_are_built_into() {
        let id = array("id");
        let mut other_id = id;
        other_id[31] ^= 1;
        let record = enroll(id, &[0, 1]).unwrap();
        let output2 = output(2);
        let record = record.with_key(VERSION, new_key(2, &output2)).unwrap();
        for index in 0..3 {
            let credential = vector(&format!("slot{index}_credential"));
            let stored = record.slot(&credential).unwrap().verifier;
            let own = verifier(UID, &id, &credential, &output(index)).unwrap();
            assert!(check(&stored, &own));
            for (uid, id) in [(UID + 1, &id), (UID, &other_id)] {
                let foreign = verifier(uid, id, &credential, &output(index)).unwrap();
                assert!(!check(&stored, &foreign));
            }
            assert!(record.check(&credential, &output(index)).unwrap());
            let other = output((index + 1) % 3);
            assert!(!record.check(&credential, &other).unwrap());
        }
        // A record whose ID changed under its slots decodes but opens with no key.
        let mut moved = record.encode().unwrap();
        moved[13..45].copy_from_slice(&other_id);
        let moved = Record::decode(&moved, UID).unwrap();
        for index in 0..3 {
            let credential = vector(&format!("slot{index}_credential"));
            assert!(!moved.check(&credential, &output(index)).unwrap());
        }
        // The same keys built into that ID open it.
        let rebuilt = enroll(other_id, &[0, 1, 2]).unwrap();
        assert!(rebuilt.check(b"A", &output(2)).unwrap());
        assert_ne!(rebuilt.encode().unwrap(), record.encode().unwrap());
    }

    #[test]
    fn adding_and_removing_keys_keep_the_record_identity() {
        let id = array("id");
        let two = enroll(id, &[0, 1]).unwrap();
        let two_bytes = two.encode().unwrap();
        let output2 = output(2);
        let three = two.with_key(VERSION, new_key(2, &output2)).unwrap();
        assert_eq!(three.encode().unwrap(), vector("record"));
        assert_eq!(three.digest().to_vec(), vector("digest"));
        assert_eq!((three.uid(), three.id()), (UID, &id));
        let credential2 = vector("slot2_credential");
        let back = three.without(VERSION, &[&credential2]).unwrap();
        assert_eq!(back.encode().unwrap(), two_bytes);
        assert_eq!(back.digest(), crypto::digest(&two_bytes));
        let credential0 = vector("slot0_credential");
        let credential1 = vector("slot1_credential");
        let one = fixture()
            .without(VERSION, &[&credential0, &credential2])
            .unwrap();
        assert_eq!(one.slots().len(), 1);
        assert_eq!(one.slots()[0].credential(), credential1.as_slice());
        assert!(one.check(&credential1, &output(1)).unwrap());
        let decoded = Record::decode(&one.encode().unwrap(), UID).unwrap();
        assert_eq!(decoded.digest(), one.digest());
    }

    #[test]
    fn mutations_refuse_invalid_results() {
        let zero = [0; 32];
        let output0 = output(0);
        assert_eq!(
            fixture()
                .with_key(VERSION, new_key(0, &output0))
                .err()
                .unwrap(),
            "login credential is already enrolled"
        );
        assert_eq!(
            fixture()
                .with_key(2, synthetic(b"new", &zero))
                .err()
                .unwrap(),
            "unknown login record version"
        );
        assert!(fixture().with_key(VERSION, synthetic(b"", &zero)).is_err());
        let credentials: Vec<[u8; 1]> = (1u8..=8).map(|i| [i]).collect();
        let eight = credentials.iter().map(|c| synthetic(c, &zero)).collect();
        let full = Record::enroll(UID, array("id"), VERSION, eight).unwrap();
        assert_eq!(
            full.with_key(VERSION, synthetic(&[9], &zero))
                .err()
                .unwrap(),
            "login record already holds eight keys"
        );
        let credential0 = vector("slot0_credential");
        let credential1 = vector("slot1_credential");
        let credential2 = vector("slot2_credential");
        for (credentials, error) in [
            (&[][..], "no login key selected for removal"),
            (&[&b"unknown"[..]][..], "login credential is not enrolled"),
            (
                &[&credential0[..], &credential0[..]][..],
                "login key selected twice for removal",
            ),
            (
                &[&credential0[..], &credential1[..], &credential2[..]][..],
                "removing every login key unlinks the record instead",
            ),
        ] {
            assert_eq!(
                fixture().without(VERSION, credentials).err().unwrap(),
                error
            );
        }
        assert_eq!(
            fixture().without(2, &[&credential0]).err().unwrap(),
            "unknown login record version"
        );
    }

    #[test]
    fn writers_choose_the_lowest_version_both_deployments_read() {
        assert_eq!(write_version(&[1], &[1]), Ok(1));
        assert_eq!(write_version(&[1, 2], &[1]), Ok(1));
        assert_eq!(write_version(&[3, 1], &[2, 1]), Ok(1));
        assert!(write_version(&[1], &[]).is_err());
        assert!(write_version(&[], &[1]).is_err());
        assert!(write_version(&[2], &[1]).is_err());
        assert!(write_version(&[2], &[2]).is_err(), "unknown to this build");
        assert_eq!(lowest_common(&[1, 2, 3], &[3, 2], &[2, 3]), Some(2));
        assert_eq!(lowest_common(&[2, 1], &[1, 2], &[2, 1]), Some(1));
        assert_eq!(lowest_common(&[1, 2], &[2], &[1]), None);
        assert_eq!(lowest_common(&[1], &[1, 2], &[2]), None);
    }
}
