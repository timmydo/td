//! PIN-authorized sealing (DESIGN.md "Salted and PIN-authorized
//! sessions"): the PolicyAuthValue policy, sealing an authValue with the
//! sensitive area encrypted, unsealing in a salted HMAC policy session,
//! the typed authorization refusals and the dictionary-attack properties.

use super::session::{trim_auth, DECRYPT, ENCRYPT, HMAC_SESSION, MAX_AUTH_VALUE};
use super::{
    creation, digest, policy_digest_from, put16, put32, put_blob, sealed_policy, zero, Client,
    PcrPolicy, PcrSelection, Reader, Refusal, SealedObject, Transport, UnsealError, ALG_NULL,
    CREATE, MAX_SEALED_PAYLOAD, POLICY_SESSION, SEALED_ATTRIBUTES, SHA256, UNSEAL,
};

pub const POLICY_AUTH_VALUE: u32 = 0x16b;
pub const GET_CAPABILITY: u32 = 0x17a;
/// TPMA_OBJECT's noDA: clear, a wrong authValue counts against the
/// TPM's dictionary-attack lockout.
pub const NO_DA: u32 = 0x400;
/// `SEALED_ATTRIBUTES` with noDA clear.
pub const DA_SEALED_ATTRIBUTES: u32 = SEALED_ATTRIBUTES & !NO_DA;
/// TPM_RC_AUTH_FAIL on session 1, the only session td sends: a wrong
/// authValue (RC_FMT1 0x08e, TPM_RC_S 0x800, TPM_RC_1 0x100).
pub const RC_AUTH_FAIL: u32 = 0x98e;
/// TPM_RC_LOCKOUT, a warning: the TPM is in dictionary-attack lockout.
pub const RC_LOCKOUT: u32 = 0x921;
const CAP_TPM_PROPERTIES: u32 = 6;
pub const PT_PERMANENT: u32 = 0x200;
pub const PT_LOCKOUT_COUNTER: u32 = 0x20e;
pub const PT_MAX_AUTH_FAIL: u32 = 0x20f;
pub const PT_LOCKOUT_INTERVAL: u32 = 0x210;
pub const PT_LOCKOUT_RECOVERY: u32 = 0x211;
/// TPMA_PERMANENT's lockoutAuthSet and inLockout.
const LOCKOUT_AUTH_SET: u32 = 1 << 2;
const IN_LOCKOUT: u32 = 1 << 9;

/// PolicyPCR(selection, pcr_digest), PolicyAuthValue, then
/// PolicyCommandCode(Unseal), from the zero digest.
pub fn auth_policy_digest(selection: PcrSelection, pcr_digest: &[u8; 32]) -> [u8; 32] {
    policy_digest_from(selection, pcr_digest, true)
}

impl PcrPolicy {
    /// This PCR state's policy with PolicyAuthValue before the command
    /// code: the object's authValue must also authorize the Unseal.
    pub fn auth_digest(&self) -> [u8; 32] {
        auth_policy_digest(self.selection, &self.pcr_digest)
    }
}

/// The authorization refusals a consumer types apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthRefusal {
    /// `RC_AUTH_FAIL`: the authValue was wrong, and the TPM counted it.
    AuthFail,
    /// `RC_LOCKOUT`: the TPM is locked out and checked nothing.
    Lockout,
}

impl Refusal {
    pub fn authorization(&self) -> Option<AuthRefusal> {
        match self.rc {
            RC_AUTH_FAIL => Some(AuthRefusal::AuthFail),
            RC_LOCKOUT => Some(AuthRefusal::Lockout),
            _ => None,
        }
    }
}

impl UnsealError {
    /// The authorization refusal that ended the unseal, if one did.
    pub fn authorization(&self) -> Option<AuthRefusal> {
        self.refusal.as_ref().and_then(Refusal::authorization)
    }
}

/// An object `seal_with_auth` sealed, and the Name of the storage primary
/// it was sealed under, for the consumer to record and require.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthSealed {
    pub object: SealedObject,
    pub primary_name: Vec<u8>,
}

/// The TPM's dictionary-attack state: TPMA_PERMANENT's lockoutAuthSet and
/// inLockout, and the four TPM_PT_LOCKOUT properties.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DictionaryAttack {
    pub lockout_auth_set: bool,
    pub in_lockout: bool,
    pub counter: u32,
    pub max_tries: u32,
    pub interval: u32,
    pub recovery: u32,
}

/// The SHA-256 Name of a public area.
pub(crate) fn object_name(public: &[u8]) -> Vec<u8> {
    let mut name = SHA256.to_be_bytes().to_vec();
    name.extend_from_slice(&digest(public));
    name
}

/// Refuse any sealed public area other than a SHA-256 keyed-hash object
/// with `SEALED_ATTRIBUTES` or `DA_SEALED_ATTRIBUTES` and exactly `policy`
/// as its authPolicy; return its attributes.
pub fn validate_auth_sealed_public(public: &[u8], policy: &[u8; 32]) -> Result<u32, String> {
    for attributes in [SEALED_ATTRIBUTES, DA_SEALED_ATTRIBUTES] {
        if sealed_policy(public, attributes).is_ok_and(|found| found == policy) {
            return Ok(attributes);
        }
    }
    Err("sealed TPM object does not have the expected PIN policy".into())
}

/// Refuse an authValue longer than the session hash, or one that is empty
/// once its trailing zeros are trimmed: the TPM would then check nothing.
const OTHER_PRIMARY: &str = "TPM storage primary is not the one recorded";

fn bounded_auth(auth: &[u8]) -> Result<(), String> {
    if auth.len() > MAX_AUTH_VALUE {
        return Err("a TPM authValue holds at most 32 bytes".into());
    }
    if trim_auth(auth).is_empty() {
        return Err("a TPM authValue must not be empty once trimmed".into());
    }
    Ok(())
}

impl<T: Transport> Client<T> {
    /// Seal `payload` under `policy`'s `auth_digest` beneath the
    /// unpersonalized storage primary, with `auth` as its authValue and
    /// `attributes` either `SEALED_ATTRIBUTES` or `DA_SEALED_ATTRIBUTES`.
    /// The Create runs in a session salted to that primary with its
    /// sensitive area, the authValue and payload, encrypted. When
    /// `expected_primary` is given, a primary with another Name is refused
    /// before the session is salted to it, so nothing secret is encrypted
    /// to a primary the caller did not record. `payload` is zeroed as soon
    /// as it is marshaled and on every return path; `auth` stays the
    /// caller's to zero.
    pub fn seal_with_auth(
        mut self,
        policy: &PcrPolicy,
        expected_primary: Option<&[u8]>,
        auth: &[u8],
        attributes: u32,
        payload: &mut [u8],
    ) -> Result<AuthSealed, String> {
        let refused = if payload.is_empty() || payload.len() > MAX_SEALED_PAYLOAD {
            Err("sealed TPM payload must hold 1 to 128 bytes".into())
        } else if attributes != SEALED_ATTRIBUTES && attributes != DA_SEALED_ATTRIBUTES {
            Err("sealed TPM attributes are SEALED_ATTRIBUTES, with or without noDA".into())
        } else {
            bounded_auth(auth)
        };
        if let Err(error) = refused {
            zero(payload);
            return Err(error);
        }
        let sealed = self.seal_in_session(policy, expected_primary, auth, attributes, payload);
        zero(payload);
        sealed
    }

    fn seal_in_session(
        &mut self,
        policy: &PcrPolicy,
        expected_primary: Option<&[u8]>,
        auth: &[u8],
        attributes: u32,
        payload: &mut [u8],
    ) -> Result<AuthSealed, String> {
        let digest = policy.auth_digest();
        let trial = self.start_session(super::TRIAL_SESSION)?;
        self.satisfy(trial, policy, true)?;
        let primary = self.primary(None)?;
        if expected_primary.is_some_and(|name| name != primary.name) {
            return Err(OTHER_PRIMARY.into());
        }
        let mut session =
            self.salted_session(primary.handle, &primary.x, &primary.y, HMAC_SESSION)?;
        let mut public = Vec::new();
        put16(&mut public, 8);
        put16(&mut public, SHA256);
        put32(&mut public, attributes);
        put_blob(&mut public, &digest)?;
        put16(&mut public, ALG_NULL);
        put_blob(&mut public, &[])?;
        // TPM2B_SENSITIVE_CREATE: the trimmed userAuth, then the payload.
        let auth = trim_auth(auth);
        let mut sensitive = Vec::with_capacity(4 + auth.len() + payload.len());
        let marshaled =
            put_blob(&mut sensitive, auth).and_then(|()| put_blob(&mut sensitive, payload));
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
        // The primary's authValue is empty, so the session key alone keys
        // the HMAC and the encryption.
        let result = self.call_session(
            CREATE,
            &[primary.handle],
            &[&primary.name],
            &mut session,
            &[],
            DECRYPT,
            &parameters,
        );
        zero(&mut parameters);
        let out = result?;
        let mut reader = Reader(&out);
        let private = reader.blob()?.to_vec();
        let public = reader.blob()?.to_vec();
        creation(&mut reader)?;
        reader.end()?;
        if sealed_policy(&public, attributes)? != digest {
            return Err("TPM changed the sealed object's policy".into());
        }
        Ok(AuthSealed {
            object: SealedObject { public, private },
            primary_name: primary.name,
        })
    }

    /// Unseal an object `seal_with_auth` sealed under `policy`, beneath the
    /// storage primary whose Name is `primary_name`, with `auth`. A primary
    /// with another Name is refused before the object is loaded and before
    /// any authValue is used. The Unseal runs in a policy session salted to
    /// that primary, its HMAC keyed by the session key and the trimmed
    /// `auth`, its reply verified before it is decrypted. A wrong `auth`
    /// is the error's `AuthRefusal::AuthFail`, a locked-out TPM its
    /// `AuthRefusal::Lockout`. The caller owns zeroing the payload.
    pub fn unseal_with_auth(
        mut self,
        policy: &PcrPolicy,
        primary_name: &[u8],
        auth: &[u8],
        public: &[u8],
        private: &[u8],
    ) -> Result<Vec<u8>, UnsealError> {
        let unsealed =
            self.unseal_with_auth_in_session(policy, primary_name, auth, public, private);
        unsealed.map_err(|message| UnsealError {
            refusal: self.refused,
            message,
        })
    }

    fn unseal_with_auth_in_session(
        &mut self,
        policy: &PcrPolicy,
        primary_name: &[u8],
        auth: &[u8],
        public: &[u8],
        private: &[u8],
    ) -> Result<Vec<u8>, String> {
        self.refused = None;
        bounded_auth(auth)?;
        if sealed_policy(public, SEALED_ATTRIBUTES)
            .or_else(|_| sealed_policy(public, DA_SEALED_ATTRIBUTES))
            .is_err()
        {
            return Err("sealed TPM object does not have the sealed format".into());
        }
        let primary = self.primary(None)?;
        if primary.name != primary_name {
            return Err(OTHER_PRIMARY.into());
        }
        let handle = self.load(primary.handle, public, private)?;
        let mut session =
            self.salted_session(primary.handle, &primary.x, &primary.y, POLICY_SESSION)?;
        self.satisfy(session.handle, policy, true)?;
        let name = object_name(public);
        let mut out = self.call_session(
            UNSEAL,
            &[handle],
            &[&name],
            &mut session,
            auth,
            ENCRYPT,
            &[],
        )?;
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

    /// The dictionary-attack state, by two TPM2_GetCapability reads of
    /// TPM_CAP_TPM_PROPERTIES, each reply holding exactly the properties
    /// asked for. Needs no authorization.
    pub fn dictionary_attack(&mut self) -> Result<DictionaryAttack, String> {
        let permanent = self.properties(PT_PERMANENT, 1)?;
        let lockout = self.properties(PT_LOCKOUT_COUNTER, 4)?;
        match (permanent.as_slice(), lockout.as_slice()) {
            ([permanent], [counter, max_tries, interval, recovery]) => Ok(DictionaryAttack {
                lockout_auth_set: permanent & LOCKOUT_AUTH_SET != 0,
                in_lockout: permanent & IN_LOCKOUT != 0,
                counter: *counter,
                max_tries: *max_tries,
                interval: *interval,
                recovery: *recovery,
            }),
            _ => Err("TPM returned other properties".into()),
        }
    }

    /// The values of `count` consecutive TPM properties from `first`.
    fn properties(&mut self, first: u32, count: u32) -> Result<Vec<u32>, String> {
        let mut parameters = Vec::with_capacity(12);
        put32(&mut parameters, CAP_TPM_PROPERTIES);
        put32(&mut parameters, first);
        put32(&mut parameters, count);
        let (_, out) = self.call(GET_CAPABILITY, &[], None, &parameters, false)?;
        let mut reader = Reader(&out);
        if reader.u8()? > 1 || reader.u32()? != CAP_TPM_PROPERTIES || reader.u32()? != count {
            return Err("TPM did not return the TPM properties asked for".into());
        }
        let mut values = Vec::new();
        for offset in 0..count {
            if Some(reader.u32()?) != first.checked_add(offset) {
                return Err("TPM did not return the TPM properties asked for".into());
            }
            values.push(reader.u32()?);
        }
        reader.end()?;
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::{response, vector};
    use crate::{
        FLUSH_CONTEXT, LOAD, NO_SESSIONS, POLICY_COMMAND_CODE, POLICY_GET_DIGEST, POLICY_PCR,
        SESSIONS,
    };

    /// The tpm-pin policy's digest is a persisted contract: td-protector
    /// pins the same literal over its own computation.
    #[test]
    fn auth_policy_digest_matches_the_vector_and_differs_from_the_pcr_policy() {
        let policy = PcrPolicy {
            selection: PcrSelection::new(1 << 4 | 1 << 9 | 1 << 12).unwrap(),
            pcr_digest: crate::pcr_digest(&[[0x44; 32], [0x49; 32], [0; 32]]),
        };
        assert_eq!(policy.pcr_digest.to_vec(), vector("pcr_digest"));
        assert_eq!(policy.auth_digest().to_vec(), vector("auth_policy"));
        assert_eq!(
            policy.auth_digest(),
            auth_policy_digest(policy.selection, &policy.pcr_digest)
        );
        assert_ne!(policy.auth_digest(), policy.digest());
        // The PCR-only policy is unchanged by the shared builder.
        let pcr7 = PcrPolicy {
            selection: PcrSelection::new(1 << 7).unwrap(),
            pcr_digest: [6; 32],
        };
        assert_eq!(
            pcr7.digest(),
            crate::policy_digest(pcr7.selection, &[6; 32])
        );
    }

    #[test]
    fn authorization_refusals_are_typed() {
        let refusal = |command, rc| Refusal { command, rc };
        assert_eq!(
            refusal(UNSEAL, 0x98e).authorization(),
            Some(AuthRefusal::AuthFail)
        );
        assert_eq!(
            refusal(UNSEAL, 0x921).authorization(),
            Some(AuthRefusal::Lockout)
        );
        // Another session's AUTH_FAIL, BAD_AUTH, POLICY_FAIL and VALUE are
        // not the typed kinds.
        for rc in [0x8e, 0xa8e, 0x9a2, 0x99d, 0x1c4, 0x922] {
            assert_eq!(refusal(UNSEAL, rc).authorization(), None, "{rc:#x}");
        }
        let error = |refusal| UnsealError {
            refusal,
            message: String::new(),
        };
        assert_eq!(
            error(Some(refusal(UNSEAL, RC_LOCKOUT))).authorization(),
            Some(AuthRefusal::Lockout)
        );
        assert_eq!(error(None).authorization(), None);
    }

    #[test]
    fn dictionary_attack_reads_exactly_the_properties_asked() {
        struct Properties {
            sent: Vec<Vec<u8>>,
            permanent: Vec<u8>,
            lockout: Vec<u8>,
        }
        impl Transport for Properties {
            fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
                self.sent.push(command.to_vec());
                Ok(if self.sent.len() == 1 {
                    self.permanent.clone()
                } else {
                    self.lockout.clone()
                })
            }
        }
        let reply = |more: u8, properties: &[(u32, u32)]| {
            let mut body = vec![more];
            put32(&mut body, CAP_TPM_PROPERTIES);
            put32(&mut body, properties.len() as u32);
            for (property, value) in properties {
                put32(&mut body, *property);
                put32(&mut body, *value);
            }
            response(NO_SESSIONS, 0, &body)
        };
        let lockout = [
            (PT_LOCKOUT_COUNTER, 3),
            (PT_MAX_AUTH_FAIL, 32),
            (PT_LOCKOUT_INTERVAL, 600),
            (PT_LOCKOUT_RECOVERY, 86400),
        ];
        let read = |permanent: Vec<u8>, lockout: Vec<u8>| {
            let mut client = Client::new(Properties {
                sent: Vec::new(),
                permanent,
                lockout,
            });
            let state = client.dictionary_attack();
            (state, std::mem::take(&mut client.transport.sent))
        };
        let (state, sent) = read(reply(1, &[(PT_PERMANENT, 0x204)]), reply(1, &lockout));
        assert_eq!(
            state.unwrap(),
            DictionaryAttack {
                lockout_auth_set: true,
                in_lockout: true,
                counter: 3,
                max_tries: 32,
                interval: 600,
                recovery: 86400,
            }
        );
        assert_eq!(
            sent,
            [vector("capability_permanent"), vector("capability_lockout")]
        );
        let (state, _) = read(reply(0, &[(PT_PERMANENT, 0x103)]), reply(0, &lockout));
        let state = state.unwrap();
        assert!(!state.lockout_auth_set && !state.in_lockout);
        let mut skipped = lockout;
        skipped[1].0 += 1;
        let mut extra = lockout.to_vec();
        extra.push((0x212, 0));
        for (permanent, lockout) in [
            (reply(2, &[(PT_PERMANENT, 0)]), reply(0, &lockout)),
            (reply(0, &[(PT_PERMANENT + 1, 0)]), reply(0, &lockout)),
            (reply(0, &[]), reply(0, &lockout)),
            (reply(0, &[(PT_PERMANENT, 0)]), reply(0, &skipped)),
            (reply(0, &[(PT_PERMANENT, 0)]), reply(0, &extra)),
            (reply(0, &[(PT_PERMANENT, 0)]), reply(0, &lockout[..3])),
        ] {
            assert!(read(permanent, lockout).0.is_err());
        }
    }

    #[test]
    fn auth_sealing_is_bounded_before_any_tpm_io() {
        struct NoIo;
        impl Transport for NoIo {
            fn exchange(&mut self, _: &[u8]) -> Result<Vec<u8>, String> {
                panic!("refusal reached the TPM");
            }
        }
        let policy = PcrPolicy {
            selection: PcrSelection::new(1 << 4).unwrap(),
            pcr_digest: [6; 32],
        };
        for (size, auth, attributes) in [
            (0, 32, DA_SEALED_ATTRIBUTES),
            (MAX_SEALED_PAYLOAD + 1, 32, DA_SEALED_ATTRIBUTES),
            (32, 33, DA_SEALED_ATTRIBUTES),
            (32, 32, DA_SEALED_ATTRIBUTES | 0x40),
            (32, 32, 0x492 ^ 0x80),
        ] {
            let mut payload = vec![0x42; size];
            assert!(Client::new(NoIo)
                .seal_with_auth(&policy, None, &vec![1; auth], attributes, &mut payload)
                .is_err());
            assert!(payload.iter().all(|byte| *byte == 0));
        }
        // An authValue that trims to nothing would authorize with nothing.
        for auth in [&[][..], &[0, 0][..]] {
            let mut payload = vec![0x42; 32];
            assert!(Client::new(NoIo)
                .seal_with_auth(&policy, None, auth, DA_SEALED_ATTRIBUTES, &mut payload)
                .unwrap_err()
                .contains("empty"));
            assert!(payload.iter().all(|byte| *byte == 0));
        }
        let mut public = Vec::new();
        put16(&mut public, 8);
        put16(&mut public, SHA256);
        put32(&mut public, DA_SEALED_ATTRIBUTES | 0x40);
        put_blob(&mut public, &policy.auth_digest()).unwrap();
        put16(&mut public, ALG_NULL);
        put_blob(&mut public, &[8; 32]).unwrap();
        for (public, auth) in [(&public[..], &[1; 32][..]), (&public[..9], &[1][..])] {
            let refused = Client::new(NoIo)
                .unseal_with_auth(&policy, &[0; 34], auth, public, &[1])
                .unwrap_err();
            assert_eq!(refused.refusal, None);
        }
        for auth in [&[1; 33][..], &[][..], &[0][..]] {
            let refused = Client::new(NoIo)
                .unseal_with_auth(&policy, &[0; 34], auth, &public, &[1])
                .unwrap_err();
            assert_eq!(refused.refusal, None);
        }
        // The sealed formats a consumer validates: either attribute word,
        // with exactly the policy.
        public[7] = 0x92;
        assert_eq!(
            validate_auth_sealed_public(&public, &policy.auth_digest()),
            Ok(DA_SEALED_ATTRIBUTES)
        );
        public[6] = 0x04;
        assert_eq!(
            validate_auth_sealed_public(&public, &policy.auth_digest()),
            Ok(SEALED_ATTRIBUTES)
        );
        assert!(validate_auth_sealed_public(&public, &policy.digest()).is_err());
    }

    /// A scripted TPM for `seal_with_auth` and `unseal_with_auth`: the
    /// vector primary, a Load returning the object's Name, sessions of the
    /// class their kind asks for, the policy commands, Unseal answering
    /// `unseal` and Create refusing. It records each command code and each
    /// session's tpmKey.
    struct Unsealing {
        codes: Vec<u32>,
        salted_to: Vec<u32>,
        unseal: Vec<u8>,
        public: Vec<u8>,
    }
    impl Transport for Unsealing {
        fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
            let code = u32::from_be_bytes(command[6..10].try_into().unwrap());
            self.codes.push(code);
            let mut body = Vec::new();
            let session = |body: &mut Vec<u8>, parameters: &[u8]| {
                put32(body, parameters.len() as u32);
                body.extend_from_slice(parameters);
                body.extend_from_slice(&[0, 0, 1, 0, 0]);
            };
            match code {
                crate::CREATE_PRIMARY => {
                    let mut parameters = Vec::new();
                    let primary = vector("primary_public");
                    put_blob(&mut parameters, &primary).unwrap();
                    put_blob(&mut parameters, &[]).unwrap();
                    put_blob(&mut parameters, &[4; 32]).unwrap();
                    put16(&mut parameters, 0x8021);
                    put32(&mut parameters, crate::OWNER);
                    put_blob(&mut parameters, &[5; 32]).unwrap();
                    put_blob(&mut parameters, &vector("primary_name")).unwrap();
                    put32(&mut body, 0x8000_0000);
                    session(&mut body, &parameters);
                    return Ok(response(SESSIONS, 0, &body));
                }
                LOAD => {
                    let mut parameters = Vec::new();
                    put_blob(&mut parameters, &object_name(&self.public)).unwrap();
                    put32(&mut body, 0x8000_0001);
                    session(&mut body, &parameters);
                    return Ok(response(SESSIONS, 0, &body));
                }
                crate::START_AUTH_SESSION => {
                    let key = u32::from_be_bytes(command[10..14].try_into().unwrap());
                    self.salted_to.push(key);
                    // The kind precedes AES-128-CFB and SHA-256 when salted,
                    // the NULL symmetric and SHA-256 when not.
                    let kind = command[command.len() - if key == crate::NULL { 5 } else { 9 }];
                    let class = if kind == HMAC_SESSION {
                        0x0200_0000
                    } else {
                        0x0300_0000
                    };
                    put32(&mut body, class);
                    put_blob(&mut body, &[0x0e; 32]).unwrap();
                }
                POLICY_GET_DIGEST => put_blob(&mut body, &vector("auth_policy")).unwrap(),
                POLICY_PCR | POLICY_AUTH_VALUE | POLICY_COMMAND_CODE | FLUSH_CONTEXT => {}
                UNSEAL => return Ok(self.unseal.clone()),
                CREATE => return Ok(response(NO_SESSIONS, 0x9a2, &[])),
                _ => panic!("unscripted {code:#x}"),
            }
            Ok(response(NO_SESSIONS, 0, &body))
        }
    }

    #[test]
    fn unseal_with_auth_types_the_auth_fail_and_lockout_replies() {
        let policy = PcrPolicy {
            selection: PcrSelection::new(1 << 4 | 1 << 9 | 1 << 12).unwrap(),
            pcr_digest: vector("pcr_digest").try_into().unwrap(),
        };
        let public = vector("object_public");
        let run = |unseal: Vec<u8>, name: &[u8]| {
            let mut client = Client::new(Unsealing {
                codes: Vec::new(),
                salted_to: Vec::new(),
                unseal,
                public: public.clone(),
            });
            let unsealed =
                client.unseal_with_auth_in_session(&policy, name, &vector("auth"), &public, &[1]);
            let refused = client.refused;
            // Salted to the primary, not NULL.
            assert!(client
                .transport
                .salted_to
                .iter()
                .all(|key| *key == 0x8000_0000));
            (
                unsealed,
                refused,
                std::mem::take(&mut client.transport.codes),
                client.handles.contains(&0x0300_0000),
            )
        };
        let name = vector("primary_name");
        for (rc, kind) in [
            (RC_AUTH_FAIL, AuthRefusal::AuthFail),
            (RC_LOCKOUT, AuthRefusal::Lockout),
        ] {
            let (unsealed, refused, codes, _) = run(response(NO_SESSIONS, rc, &[]), &name);
            assert_eq!(
                unsealed.unwrap_err(),
                format!("TPM command 0x15e refused: {rc:#x}")
            );
            let refused = refused.unwrap();
            assert_eq!(
                refused,
                Refusal {
                    command: UNSEAL,
                    rc
                }
            );
            assert_eq!(refused.authorization(), Some(kind));
            assert_eq!(
                codes,
                [
                    crate::CREATE_PRIMARY,
                    LOAD,
                    crate::START_AUTH_SESSION,
                    POLICY_PCR,
                    POLICY_AUTH_VALUE,
                    POLICY_COMMAND_CODE,
                    POLICY_GET_DIGEST,
                    UNSEAL
                ]
            );
        }
        // Another primary's Name is refused before Load, with no refusal.
        let mut other = name.clone();
        other[2] ^= 1;
        let (unsealed, refused, codes, _) = run(response(NO_SESSIONS, RC_AUTH_FAIL, &[]), &other);
        assert!(unsealed.unwrap_err().contains("not the one"));
        assert_eq!(refused, None);
        assert_eq!(codes, [crate::CREATE_PRIMARY]);
        // A success reply whose HMAC does not verify is not trusted, and
        // its session stays owned for drop to flush.
        let (unsealed, refused, _, owned) = run(vector("unseal_reply"), &name);
        assert!(unsealed.unwrap_err().contains("HMAC"));
        assert_eq!(refused, None);
        assert!(owned);
    }

    #[test]
    fn seal_with_auth_refuses_another_primary_before_salting_to_it() {
        let policy = PcrPolicy {
            selection: PcrSelection::new(1 << 4 | 1 << 9 | 1 << 12).unwrap(),
            pcr_digest: vector("pcr_digest").try_into().unwrap(),
        };
        let run = |expected: Option<&[u8]>| {
            let mut client = Client::new(Unsealing {
                codes: Vec::new(),
                salted_to: Vec::new(),
                unseal: Vec::new(),
                public: Vec::new(),
            });
            let mut payload = [0x42; 32];
            let sealed = client.seal_in_session(
                &policy,
                expected,
                &vector("auth"),
                DA_SEALED_ATTRIBUTES,
                &mut payload,
            );
            (
                sealed,
                std::mem::take(&mut client.transport.codes),
                std::mem::take(&mut client.transport.salted_to),
            )
        };
        let trial = [
            crate::START_AUTH_SESSION,
            POLICY_PCR,
            POLICY_AUTH_VALUE,
            POLICY_COMMAND_CODE,
            POLICY_GET_DIGEST,
            crate::CREATE_PRIMARY,
        ];
        let name = vector("primary_name");
        let mut other = name.clone();
        other[2] ^= 1;
        let (sealed, codes, salted_to) = run(Some(&other));
        assert!(sealed.unwrap_err().contains("storage primary"));
        assert_eq!(codes, trial);
        assert_eq!(salted_to, [crate::NULL]);
        // The recorded primary, or none, goes on to the salted Create.
        for expected in [Some(&name[..]), None] {
            let (sealed, codes, salted_to) = run(expected);
            assert_eq!(sealed.unwrap_err(), "TPM command 0x153 refused: 0x9a2");
            assert_eq!(codes[..6], trial);
            assert_eq!(codes[6..], [crate::START_AUTH_SESSION, CREATE]);
            assert_eq!(salted_to, [crate::NULL, 0x8000_0000]);
        }
    }
}
