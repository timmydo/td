//! Token capability negotiation and enrollment that requires proof of possession.

use super::fido_cbor::{self as cbor, Encoder, Value};
use super::fido_ctap::{AssertionRequest, Es256PublicKey, MAX_CREDENTIAL_ID, RP_ID};
use super::{crypto, tpm};

pub const GET_INFO: &[u8] = &[4];

pub struct Info {
    aaguid: [u8; 16],
    max_message: usize,
    max_id: usize,
    can_enroll: bool,
}

impl Info {
    /// Capabilities are untrusted negotiation data, never token identity or consent.
    pub fn parse(response: &[u8]) -> Result<Self, String> {
        let value = response_value(response)?;
        let versions = array(value.required(&Value::Unsigned(1))?)?;
        let mut ctap2 = false;
        let mut modern = false;
        for version in versions {
            let version = version.text()?;
            ctap2 |= matches!(version, "FIDO_2_0" | "FIDO_2_1" | "FIDO_2_3");
            modern |= matches!(version, "FIDO_2_1" | "FIDO_2_3");
        }
        if !ctap2 {
            return Err("token does not advertise a supported CTAP2 version".into());
        }
        let aaguid = value
            .required(&Value::Unsigned(3))?
            .bytes()?
            .try_into()
            .map_err(|_| "invalid getInfo AAGUID")?;
        let mut up = true;
        let mut platform = false;
        let mut protected = false;
        let mut always_uv = false;
        let mut make_without_uv = false;
        if let Some(options) = value.get(&Value::Unsigned(4))? {
            for (key, value) in options.map()? {
                let enabled = boolean(value)?;
                match key.text()? {
                    "up" => up = enabled,
                    "plat" => platform = enabled,
                    "uv" | "clientPin" => protected |= enabled,
                    "alwaysUv" => always_uv = enabled,
                    "makeCredUvNotRqd" => make_without_uv = enabled,
                    _ => {}
                }
            }
        }
        if !up {
            return Err("token does not support user presence".into());
        }
        if platform {
            return Err("token is platform-attached; a removable token is required".into());
        }
        if always_uv {
            return Err("token alwaysUv policy requires unsupported user verification".into());
        }
        if let Some(algorithms) = value.get(&Value::Unsigned(10))? {
            let mut es256 = false;
            for algorithm in array(algorithms)? {
                let kind = algorithm.required(&Value::Text("type"))?.text()?;
                let alg = algorithm.required(&Value::Text("alg"))?;
                if !matches!(alg, Value::Unsigned(_) | Value::Negative(_)) {
                    return Err("invalid getInfo signature algorithm".into());
                }
                es256 |= kind == "public-key" && alg == &Value::Negative(6);
            }
            if !es256 {
                return Err("token does not advertise ES256".into());
            }
        }
        if let Some(count) = value.get(&Value::Unsigned(7))? {
            if count.unsigned()? == 0 {
                return Err("zero credential-list capacity".into());
            }
        }
        Ok(Self {
            aaguid,
            max_message: limit(&value, 5, 1024, cbor::MAX_BYTES)?,
            max_id: limit(&value, 8, MAX_CREDENTIAL_ID, MAX_CREDENTIAL_ID)?,
            can_enroll: !protected || (modern && make_without_uv),
        })
    }

    pub fn max_message(&self) -> usize {
        self.max_message
    }

    pub fn assertion(&self, id: &[u8], challenge: [u8; 32]) -> Result<AssertionRequest, String> {
        self.check_id(id)?;
        AssertionRequest::new(id, challenge, self.max_message)
    }

    fn check_id(&self, id: &[u8]) -> Result<(), String> {
        if id.is_empty() || id.len() > self.max_id {
            return Err("credential ID exceeds token profile".into());
        }
        Ok(())
    }
}

fn array<'a, 'b>(value: &'b Value<'a>) -> Result<&'b [Value<'a>], String> {
    match value {
        Value::Array(values) if !values.is_empty() => Ok(values),
        _ => Err("expected nonempty CBOR array".into()),
    }
}

fn boolean(value: &Value<'_>) -> Result<bool, String> {
    match value {
        Value::Simple(20) => Ok(false),
        Value::Simple(21) => Ok(true),
        _ => Err("expected CBOR boolean".into()),
    }
}

fn limit(value: &Value<'_>, key: u64, default: usize, ceiling: usize) -> Result<usize, String> {
    let Some(value) = value.get(&Value::Unsigned(key))? else {
        return Ok(default);
    };
    let number = value.unsigned()?;
    if number == 0 {
        return Err("zero authenticator size limit".into());
    }
    usize::try_from(number.min(ceiling as u64)).map_err(|_| "size limit overflow".into())
}

fn response_value(response: &[u8]) -> Result<Value<'_>, String> {
    if response.len() > cbor::MAX_BYTES {
        return Err("CTAP response byte limit".into());
    }
    let (&status, bytes) = response.split_first().ok_or("missing CTAP status")?;
    if status != 0 {
        return Err(format!("CTAP enrollment refused: {status:#04x}"));
    }
    cbor::decode(bytes)
}

pub struct MakeCredential {
    info: Info,
    challenge: [u8; 32],
    excluded: Vec<u8>,
    bytes: Vec<u8>,
}

impl Drop for MakeCredential {
    fn drop(&mut self) {
        self.challenge.fill(0);
        self.excluded.fill(0);
        self.bytes.fill(0);
    }
}

impl MakeCredential {
    /// Both hash and opaque user handle must come from the trusted fresh operation.
    pub fn primary(info: Info, challenge: [u8; 32], user: [u8; 32]) -> Result<Self, String> {
        Self::new(info, challenge, user, None)
    }

    /// A recovery request cannot omit the already proved primary credential.
    pub fn recovery(
        info: Info,
        challenge: [u8; 32],
        user: [u8; 32],
        primary: &Credential,
    ) -> Result<Self, String> {
        Self::new(info, challenge, user, Some(primary.id()))
    }

    fn new(
        info: Info,
        challenge: [u8; 32],
        user: [u8; 32],
        exclude: Option<&[u8]>,
    ) -> Result<Self, String> {
        if !info.can_enroll {
            return Err("token enrollment requires unsupported PIN/UV authorization".into());
        }
        if let Some(id) = exclude {
            info.check_id(id)?;
        }
        let mut out = Encoder::new();
        out.head(5, if exclude.is_some() { 6 } else { 5 })?;
        out.head(0, 1)?;
        out.bytes(&challenge)?;
        out.head(0, 2)?;
        out.head(5, 2)?;
        out.text("id")?;
        out.text(RP_ID)?;
        out.text("name")?;
        out.text("td secret store")?;
        out.head(0, 3)?;
        out.head(5, 3)?;
        out.text("id")?;
        out.bytes(&user)?;
        out.text("name")?;
        out.text("td secret store")?;
        out.text("displayName")?;
        out.text("td secret store")?;
        out.head(0, 4)?;
        out.head(4, 1)?;
        out.head(5, 2)?;
        out.text("alg")?;
        out.head(1, 6)?;
        out.text("type")?;
        out.text("public-key")?;
        if let Some(id) = exclude {
            out.head(0, 5)?;
            out.head(4, 1)?;
            out.head(5, 2)?;
            out.text("id")?;
            out.bytes(id)?;
            out.text("type")?;
            out.text("public-key")?;
        }
        out.head(0, 7)?;
        out.head(5, 1)?;
        out.text("rk")?;
        out.boolean(false)?;
        // up=true and uv=false are the defaults; unsupported option keys stay absent.
        let mut encoded = out.finish()?;
        if encoded.len() >= info.max_message {
            encoded.fill(0);
            return Err("makeCredential exceeds token message limit".into());
        }
        let mut bytes = Vec::with_capacity(encoded.len() + 1);
        bytes.push(1);
        bytes.extend_from_slice(&encoded);
        encoded.fill(0);
        Ok(Self {
            info,
            challenge,
            excluded: exclude.unwrap_or_default().to_vec(),
            bytes,
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Unsigned enrollment output cannot produce an enrolled credential on its own.
    pub fn proof(self, response: &[u8], challenge: [u8; 32]) -> Result<EnrollmentProof, String> {
        if challenge == self.challenge {
            return Err("enrollment proof must use a fresh challenge".into());
        }
        let value = response_value(response)?;
        let format = value.required(&Value::Unsigned(1))?.text()?;
        if format.is_empty() {
            return Err("empty attestation format".into());
        }
        if let Some(statement) = value.get(&Value::Unsigned(3))? {
            statement.map()?;
        }
        let data = value.required(&Value::Unsigned(2))?.bytes()?;
        if data.get(..32) != Some(crypto::digest(RP_ID.as_bytes()).as_slice()) {
            return Err("enrollment RP hash mismatch".into());
        }
        let flags = *data.get(32).ok_or("short enrollment flags")?;
        if flags & 0x41 != 0x41 || flags & 0x18 != 0 {
            return Err(
                "enrollment requires presence and a device-bound attested credential".into(),
            );
        }
        let aaguid = data.get(37..53).ok_or("short enrollment AAGUID")?;
        if aaguid != self.info.aaguid && !(format == "none" && aaguid == [0; 16]) {
            return Err("enrollment AAGUID changed from getInfo".into());
        }
        let size = u16::from_be_bytes(
            data.get(53..55)
                .ok_or("short credential length")?
                .try_into()
                .map_err(|_| "invalid credential length")?,
        ) as usize;
        let id = data
            .get(55..55 + size)
            .ok_or("short enrollment credential ID")?;
        self.info.check_id(id)?;
        if id == self.excluded {
            return Err("enrollment returned excluded credential".into());
        }
        let tail = data.get(55 + size..).ok_or("missing enrollment key")?;
        let (_, key_size) = cbor::prefix(tail)?;
        let cose = tail.get(..key_size).ok_or("short enrollment key")?;
        let key = Es256PublicKey::from_cose(cose)?;
        let extensions = tail.get(key_size..).ok_or("short enrollment extensions")?;
        if flags & 0x80 != 0 {
            for (name, _) in cbor::decode(extensions)?.map()? {
                name.text()?;
            }
        } else if !extensions.is_empty() {
            return Err("unexpected enrollment authenticator data".into());
        }
        let request = self.info.assertion(id, challenge)?;
        Ok(EnrollmentProof {
            request,
            key,
            credential: Credential {
                id: id.to_vec(),
                cose: cose.to_vec(),
            },
        })
    }
}

pub struct EnrollmentProof {
    request: AssertionRequest,
    key: Es256PublicKey,
    credential: Credential,
}

impl EnrollmentProof {
    pub fn bytes(&self) -> &[u8] {
        self.request.bytes()
    }

    pub fn verify<T: tpm::Transport>(
        self,
        response: &[u8],
        tpm: &mut tpm::Client<T>,
    ) -> Result<Credential, String> {
        let info = self.request.verify(response, &self.key, tpm)?;
        if info.backup_eligible || info.backed_up {
            return Err("enrollment proof is not device-bound".into());
        }
        Ok(self.credential)
    }
}

/// Constructed only after a fresh, TPM-verified assertion under the returned key.
/// Persistence must bind these public bytes and the complete recovery policy.
pub struct Credential {
    id: Vec<u8>,
    cose: Vec<u8>,
}

impl Credential {
    pub fn id(&self) -> &[u8] {
        &self.id
    }
    pub fn cose(&self) -> &[u8] {
        &self.cose
    }
}

impl Drop for Credential {
    fn drop(&mut self) {
        self.id.fill(0);
        self.cose.fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(options: &[(&str, bool)], version: &str, max: u64) -> Vec<u8> {
        let mut out = Encoder::new();
        out.head(5, 4).unwrap();
        out.head(0, 1).unwrap();
        out.head(4, 1).unwrap();
        out.text(version).unwrap();
        out.head(0, 3).unwrap();
        out.bytes(&[0; 16]).unwrap();
        out.head(0, 4).unwrap();
        out.head(5, options.len() as u64).unwrap();
        for (key, value) in options {
            out.text(key).unwrap();
            out.boolean(*value).unwrap();
        }
        out.head(0, 5).unwrap();
        out.head(0, max).unwrap();
        let mut bytes = vec![0];
        bytes.extend(out.finish().unwrap());
        bytes
    }

    fn standard() -> Info {
        Info::parse(&info(&[], "FIDO_2_1", 1024)).unwrap()
    }

    fn hex(s: &str) -> Vec<u8> {
        s.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u8::from_str_radix(std::str::from_utf8(b).unwrap(), 16).unwrap())
            .collect()
    }

    fn cose() -> Vec<u8> {
        // The independently signed public assertion fixture used by fido_ctap.
        hex(concat!(
            "a5010203262001215820",
            "ab8ace3ba858575dd060bf6e790f73982165b36abbfffb86cf0f5e032fafbb5a",
            "225820552ef0c808cfa668e3012f4411fc0a3ad01a39d3a0fb158534721a8016b31553"
        ))
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

    fn request(exclude: Option<&[u8]>) -> MakeCredential {
        MakeCredential::new(standard(), [1; 32], [2; 32], exclude).unwrap()
    }

    #[test]
    fn capabilities_refuse_policy_downgrade_and_keep_default_limits() {
        assert_eq!(GET_INFO, [4]);
        let default = hex("00a20181684649444f5f325f30035000000000000000000000000000000000");
        let parsed = Info::parse(&default).unwrap();
        assert_eq!(parsed.max_message(), 1024);
        assert_eq!(parsed.max_id, 1024);
        for version in ["FIDO_2_0", "FIDO_2_1", "FIDO_2_3"] {
            assert!(Info::parse(&info(&[], version, 7609)).is_ok());
        }
        for version in ["U2F_V2", "FIDO_2_1_PRE", "FIDO_2_2", "unknown"] {
            assert!(Info::parse(&info(&[], version, 1024)).is_err());
        }
        for options in [
            &[("up", false)][..],
            &[("plat", true)],
            &[("alwaysUv", true)],
        ] {
            assert!(Info::parse(&info(options, "FIDO_2_1", 1024)).is_err());
        }
        let protected = Info::parse(&info(&[("clientPin", true)], "FIDO_2_1", 1024)).unwrap();
        assert!(protected.assertion(&[7], [0; 32]).is_ok());
        assert!(MakeCredential::new(protected, [1; 32], [2; 32], None).is_err());
        let options = [("clientPin", true), ("makeCredUvNotRqd", true)];
        let modern = Info::parse(&info(&options, "FIDO_2_1", 1024)).unwrap();
        assert!(MakeCredential::new(modern, [1; 32], [2; 32], None).is_ok());
        let old = Info::parse(&info(&options, "FIDO_2_0", 1024)).unwrap();
        assert!(MakeCredential::new(old, [1; 32], [2; 32], None).is_err());
        assert!(Info::parse(&info(&[], "FIDO_2_1", 0)).is_err());
        assert_eq!(
            Info::parse(&info(&[], "FIDO_2_1", u64::MAX))
                .unwrap()
                .max_message(),
            7609
        );
        for size in 0..default.len() {
            assert!(Info::parse(&default[..size]).is_err());
        }
    }

    #[test]
    fn advertised_algorithm_and_credential_limits_are_enforced() {
        for (algorithm, size, count, accepted) in [
            (6, 1, 1, true),
            (7, 1, 1, false),
            (6, 0, 1, false),
            (6, 1, 0, false),
        ] {
            let mut out = Encoder::new();
            out.head(5, 5).unwrap();
            out.head(0, 1).unwrap();
            out.head(4, 1).unwrap();
            out.text("FIDO_2_1").unwrap();
            out.head(0, 3).unwrap();
            out.bytes(&[0; 16]).unwrap();
            out.head(0, 7).unwrap();
            out.head(0, count).unwrap();
            out.head(0, 8).unwrap();
            out.head(0, size).unwrap();
            out.head(0, 10).unwrap();
            out.head(4, 1).unwrap();
            out.head(5, 2).unwrap();
            out.text("alg").unwrap();
            out.head(1, algorithm).unwrap();
            out.text("type").unwrap();
            out.text("public-key").unwrap();
            let mut bytes = vec![0];
            bytes.extend(out.finish().unwrap());
            let parsed = Info::parse(&bytes);
            assert_eq!(parsed.is_ok(), accepted);
            if let Ok(parsed) = parsed {
                assert!(parsed.assertion(&[7], [3; 32]).is_ok());
                assert!(parsed.assertion(&[7, 8], [3; 32]).is_err());
                assert!(MakeCredential::new(parsed, [1; 32], [2; 32], Some(&[7, 8])).is_err());
            }
        }
        let mut bad_option = info(&[("up", true)], "FIDO_2_1", 1024);
        *bad_option.iter_mut().find(|b| **b == 0xf5).unwrap() = 1;
        assert!(Info::parse(&bad_option).is_err());
    }

    #[test]
    fn request_pins_profile_and_never_silently_discards_recovery_exclusion() {
        let request = request(Some(&[9, 8]));
        assert_eq!(request.bytes()[0], 1);
        let value = cbor::decode(&request.bytes()[1..]).unwrap();
        assert_eq!(
            value
                .required(&Value::Unsigned(1))
                .unwrap()
                .bytes()
                .unwrap(),
            [1; 32]
        );
        assert_eq!(
            value
                .required(&Value::Unsigned(2))
                .unwrap()
                .required(&Value::Text("id"))
                .unwrap()
                .text()
                .unwrap(),
            RP_ID
        );
        let options = value.required(&Value::Unsigned(7)).unwrap();
        assert_eq!(options.map().unwrap().len(), 1);
        assert!(!boolean(options.required(&Value::Text("rk")).unwrap()).unwrap());
        let excluded = array(value.required(&Value::Unsigned(5)).unwrap()).unwrap();
        assert_eq!(excluded.len(), 1);
        assert_eq!(
            excluded[0]
                .required(&Value::Text("id"))
                .unwrap()
                .bytes()
                .unwrap(),
            [9, 8]
        );
        for exclude in [&[][..], &vec![0; 1025]] {
            assert!(MakeCredential::new(standard(), [1; 32], [2; 32], Some(exclude)).is_err());
        }
        let exact = Info::parse(&info(&[], "FIDO_2_1", request.bytes().len() as u64)).unwrap();
        assert!(MakeCredential::new(exact, [1; 32], [2; 32], Some(&[9, 8])).is_ok());
        let short = Info::parse(&info(&[], "FIDO_2_1", request.bytes().len() as u64 - 1)).unwrap();
        assert!(MakeCredential::new(short, [1; 32], [2; 32], Some(&[9, 8])).is_err());
    }

    #[test]
    fn unsigned_enrollment_rejects_substitution_truncation_and_challenge_reuse() {
        let data = auth();
        let good = response(&data);
        assert!(request(None).proof(&good, [3; 32]).is_ok());
        assert!(request(None).proof(&good, [1; 32]).is_err());
        assert!(request(Some(&[7])).proof(&good, [3; 32]).is_err());
        for size in 0..data.len() {
            assert!(
                request(None)
                    .proof(&response(&data[..size]), [3; 32])
                    .is_err(),
                "{size}"
            );
        }
        for offset in [0, 37, 53, 54] {
            let mut bad = data.clone();
            bad[offset] ^= 1;
            assert!(request(None).proof(&response(&bad), [3; 32]).is_err());
        }
        for flags in [0, 1, 0x40, 0x49, 0x51, 0x59, 0xc1] {
            let mut bad = data.clone();
            bad[32] = flags;
            assert!(request(None).proof(&response(&bad), [3; 32]).is_err());
        }
        let mut future = data.clone();
        future[32] |= 0x22;
        assert!(request(None).proof(&response(&future), [3; 32]).is_ok());
        let mut tail = data;
        tail.push(0);
        assert!(request(None).proof(&response(&tail), [3; 32]).is_err());
        let mut status = good;
        status[0] = 0x19;
        assert!(request(None).proof(&status, [3; 32]).is_err());
    }

    #[test]
    fn none_attestation_can_omit_statement_and_anonymize_aaguid() {
        let mut token = standard();
        token.aaguid = [8; 16];
        let made = response(&auth());
        assert!(MakeCredential::primary(token, [1; 32], [2; 32])
            .unwrap()
            .proof(&made, [3; 32])
            .is_ok());
        let mut out = Encoder::new();
        out.head(5, 2).unwrap();
        out.head(0, 1).unwrap();
        out.text("none").unwrap();
        out.head(0, 2).unwrap();
        out.bytes(&auth()).unwrap();
        let mut missing = vec![0];
        missing.extend(out.finish().unwrap());
        assert!(request(None).proof(&missing, [3; 32]).is_ok());
        let mut wrong = auth();
        wrong[37] = 8;
        assert!(request(None).proof(&response(&wrong), [3; 32]).is_err());
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
        let emulator = tpm::tests::Emulator::start(&root);
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
        let exclude = array(wire.required(&Value::Unsigned(5)).unwrap()).unwrap();
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
        assert!(request(None)
            .proof(&made, wrong_client)
            .unwrap()
            .verify(&signed, &mut tpm)
            .is_err());
        let mut wrong_key = auth();
        *wrong_key.last_mut().unwrap() ^= 1;
        assert!(request(None)
            .proof(&response(&wrong_key), client)
            .unwrap()
            .verify(&signed, &mut tpm)
            .is_err());
        *signed.last_mut().unwrap() ^= 1;
        assert!(request(None)
            .proof(&made, client)
            .unwrap()
            .verify(&signed, &mut tpm)
            .is_err());
    }
}
