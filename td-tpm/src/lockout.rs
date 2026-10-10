//! The lockout hierarchy (DESIGN.md "Lockout hierarchy"): taking it from
//! the empty authorization with TPM2_HierarchyChangeAuth, then
//! TPM2_DictionaryAttackParameters, TPM2_DictionaryAttackLockReset and
//! TPM2_Clear under the value taken, each in an HMAC session salted to the
//! recorded storage primary.

use super::auth::AuthRefusal;
use super::session::{trim_auth, DECRYPT, HMAC_SESSION};
use super::{put16, put32, put_blob, zero, Client, Reader, Refusal, Transport, ALG_NULL};
use std::fmt;
use std::fs::File;
use std::io::Read;

pub const CLEAR: u32 = 0x126;
pub const HIERARCHY_CHANGE_AUTH: u32 = 0x129;
pub const SET_PRIMARY_POLICY: u32 = 0x12e;
pub const DICTIONARY_ATTACK_LOCK_RESET: u32 = 0x139;
pub const DICTIONARY_ATTACK_PARAMETERS: u32 = 0x13a;
/// TPM_RH_LOCKOUT. A permanent handle's Name is the handle itself.
pub const LOCKOUT: u32 = 0x4000_000a;
/// TPM_RC_DISABLED: TPM2_Clear after TPM2_ClearControl disabled it.
pub const RC_DISABLED: u32 = 0x120;

/// The lockout hierarchy's authorization: 32 bytes in one heap allocation,
/// zeroed on drop, never empty once trimmed. Its encryption under the
/// volume key is td-protector's lockout token.
pub struct LockoutAuth(Box<[u8; 32]>);

impl LockoutAuth {
    /// A fresh authorization from `/dev/random`, which blocks until the
    /// kernel's generator is initialized.
    pub fn generate() -> Result<Self, String> {
        let mut bytes = Box::new([0; 32]);
        let read = File::open("/dev/random").and_then(|mut file| file.read_exact(bytes.as_mut()));
        if let Err(error) = read {
            zero(bytes.as_mut());
            return Err(format!("read /dev/random: {error}"));
        }
        Self::boxed(bytes)
    }

    /// An authorization a consumer decrypted. `bytes` is copied and then
    /// zeroed, refused or not.
    pub fn from_bytes(bytes: &mut [u8; 32]) -> Result<Self, String> {
        let mut boxed = Box::new([0; 32]);
        boxed.copy_from_slice(bytes);
        zero(bytes);
        Self::boxed(boxed)
    }

    /// An authorization that trims to nothing would leave the hierarchy
    /// authorized by nothing.
    fn boxed(mut bytes: Box<[u8; 32]>) -> Result<Self, String> {
        if trim_auth(bytes.as_slice()).is_empty() {
            zero(bytes.as_mut());
            return Err("a lockout authorization must not be empty once trimmed".into());
        }
        Ok(Self(bytes))
    }

    pub fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Drop for LockoutAuth {
    fn drop(&mut self) {
        zero(self.0.as_mut());
    }
}

impl fmt::Debug for LockoutAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LockoutAuth(..)")
    }
}

/// TPM2_DictionaryAttackParameters' three values, in seconds but for
/// `max_tries`. Which values to set is the consumer's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DaParameters {
    pub max_tries: u32,
    pub recovery_time: u32,
    pub lockout_recovery: u32,
}

/// Why a lockout command failed. Nothing here is retried: the lockout
/// hierarchy allows one wrong authorization per `lockout_recovery`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockoutError {
    /// `take_lockout` found TPMA_PERMANENT's lockoutAuthSet set and sent
    /// nothing to the lockout hierarchy.
    AlreadySet,
    /// The storage primary's Name is not the one recorded: refused before
    /// any session is salted to it, nothing sent to the lockout hierarchy.
    OtherPrimary,
    /// The TPM refused the lockout authorization of `command`.
    /// `AuthRefusal::AuthFail` is a wrong value, after which the TPM
    /// refuses every lockout authorization until `lockout_recovery` has
    /// passed (or, with zero, until its next reset); `AuthRefusal::Lockout`
    /// is that refusal.
    Authorization { command: u32, kind: AuthRefusal },
    /// TPM2_Clear refused with `TPM_RC_DISABLED`: TPM2_ClearControl has
    /// disabled it, and only the platform hierarchy or a firmware clear
    /// enables it again.
    ClearDisabled,
    /// Anything else, with the TPM's refusal when one ended it.
    Failed {
        refusal: Option<Refusal>,
        message: String,
    },
}

impl LockoutError {
    /// The authorization refusal of a lockout command, if that ended it.
    pub fn authorization(&self) -> Option<AuthRefusal> {
        match self {
            Self::Authorization { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// A refusal of the lockout command `code` with an authorization code
    /// is typed; a refusal of any other command, such as CreatePrimary's
    /// under an owner authorization another system set, is not.
    fn from_refusal(code: u32, refusal: Option<Refusal>, message: String) -> Self {
        match refusal {
            Some(Refusal {
                command: CLEAR,
                rc: RC_DISABLED,
            }) if code == CLEAR => Self::ClearDisabled,
            Some(refused) if refused.command == code => match refused.authorization() {
                Some(kind) => Self::Authorization {
                    command: code,
                    kind,
                },
                None => Self::Failed { refusal, message },
            },
            _ => Self::Failed { refusal, message },
        }
    }
}

impl fmt::Display for LockoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadySet => f.write_str("the TPM's lockout authorization is already set"),
            Self::OtherPrimary => f.write_str("TPM storage primary is not the one recorded"),
            Self::Authorization {
                command,
                kind: AuthRefusal::AuthFail,
            } => write!(
                f,
                "TPM command {command:#x} refused: wrong lockout authorization"
            ),
            Self::Authorization {
                command,
                kind: AuthRefusal::Lockout,
            } => write!(
                f,
                "TPM command {command:#x} refused: lockout authorization locked out"
            ),
            Self::ClearDisabled => f.write_str("the TPM has TPM2_Clear disabled"),
            Self::Failed { message, .. } => f.write_str(message),
        }
    }
}

impl From<LockoutError> for String {
    fn from(error: LockoutError) -> Self {
        error.to_string()
    }
}

/// One lockout command's wire request: its code, the authorization it is
/// sent under and the one its reply is keyed with, its session attributes
/// and its parameters, zeroed on drop.
pub(crate) struct Request<'a> {
    pub(crate) code: u32,
    pub(crate) auth: &'a [u8],
    pub(crate) response_auth: &'a [u8],
    pub(crate) attributes: u8,
    pub(crate) parameters: Vec<u8>,
}

impl Drop for Request<'_> {
    fn drop(&mut self) {
        zero(&mut self.parameters);
    }
}

/// TPM2_HierarchyChangeAuth of TPM_RH_LOCKOUT from the empty authorization
/// to `new`, trimmed as the TPM stores it, sent encrypted.
pub(crate) fn change_auth(new: &LockoutAuth) -> Result<Request<'_>, String> {
    let new = trim_auth(new.bytes());
    let mut request = Request {
        code: HIERARCHY_CHANGE_AUTH,
        auth: &[],
        response_auth: new,
        attributes: DECRYPT,
        parameters: Vec::with_capacity(2 + new.len()),
    };
    put_blob(&mut request.parameters, new)?;
    Ok(request)
}

/// TPM2_SetPrimaryPolicy of TPM_RH_LOCKOUT to the empty authPolicy with
/// TPM_ALG_NULL: no policy session authorizes the hierarchy any longer.
fn empty_policy(auth: &LockoutAuth) -> Request<'_> {
    let mut parameters = Vec::with_capacity(4);
    put16(&mut parameters, 0);
    put16(&mut parameters, ALG_NULL);
    Request {
        code: SET_PRIMARY_POLICY,
        auth: auth.bytes(),
        response_auth: auth.bytes(),
        attributes: 0,
        parameters,
    }
}

fn parameters(auth: &LockoutAuth, values: DaParameters) -> Request<'_> {
    let mut parameters = Vec::with_capacity(12);
    for value in [
        values.max_tries,
        values.recovery_time,
        values.lockout_recovery,
    ] {
        put32(&mut parameters, value);
    }
    Request {
        code: DICTIONARY_ATTACK_PARAMETERS,
        auth: auth.bytes(),
        response_auth: auth.bytes(),
        attributes: 0,
        parameters,
    }
}

fn lock_reset(auth: &LockoutAuth) -> Request<'_> {
    Request {
        code: DICTIONARY_ATTACK_LOCK_RESET,
        auth: auth.bytes(),
        response_auth: auth.bytes(),
        attributes: 0,
        parameters: Vec::new(),
    }
}

/// TPM2_Clear leaves the lockout authorization empty, so its reply is
/// keyed with the empty value.
fn clear(auth: &LockoutAuth) -> Request<'_> {
    Request {
        code: CLEAR,
        auth: auth.bytes(),
        response_auth: &[],
        attributes: 0,
        parameters: Vec::new(),
    }
}

impl<T: Transport> Client<T> {
    /// Take the lockout hierarchy: with lockoutAuthSet clear, as
    /// `dictionary_attack` reads it, TPM2_HierarchyChangeAuth from the
    /// empty authorization to `new`, then `clear_lockout_policy` under
    /// `new`, each under the storage primary whose Name is `primary_name`.
    /// The take is complete only when both succeed: the authValue alone
    /// leaves a lockoutPolicy another system set able to authorize the
    /// hierarchy. With lockoutAuthSet set it is `AlreadySet` and sends
    /// nothing to the lockout hierarchy.
    pub fn take_lockout(
        &mut self,
        primary_name: &[u8],
        new: &LockoutAuth,
    ) -> Result<(), LockoutError> {
        let state = self
            .dictionary_attack()
            .map_err(|message| LockoutError::Failed {
                refusal: self.refused,
                message,
            })?;
        if state.lockout_auth_set {
            return Err(LockoutError::AlreadySet);
        }
        let request = change_auth(new).map_err(|message| LockoutError::Failed {
            refusal: None,
            message,
        })?;
        self.lockout_command(primary_name, &request)?;
        self.clear_lockout_policy(primary_name, new)
    }

    /// TPM2_SetPrimaryPolicy of TPM_RH_LOCKOUT to the empty authPolicy
    /// under `auth`, so only `auth` authorizes the hierarchy. Idempotent:
    /// a proof sends it again, repairing a take interrupted after its
    /// HierarchyChangeAuth.
    pub fn clear_lockout_policy(
        &mut self,
        primary_name: &[u8],
        auth: &LockoutAuth,
    ) -> Result<(), LockoutError> {
        self.lockout_command(primary_name, &empty_policy(auth))
    }

    /// TPM2_DictionaryAttackParameters under `auth`, which also proves it.
    pub fn set_dictionary_attack(
        &mut self,
        primary_name: &[u8],
        auth: &LockoutAuth,
        values: DaParameters,
    ) -> Result<(), LockoutError> {
        self.lockout_command(primary_name, &parameters(auth, values))
    }

    /// TPM2_DictionaryAttackLockReset under `auth`: the failure counter to
    /// zero and the TPM out of its dictionary-attack lockout.
    pub fn reset_lockout(
        &mut self,
        primary_name: &[u8],
        auth: &LockoutAuth,
    ) -> Result<(), LockoutError> {
        self.lockout_command(primary_name, &lock_reset(auth))
    }

    /// TPM2_Clear under `auth`. The TPM changes its storage seed, so every
    /// object sealed under the storage primary is lost, and empties the
    /// lockout authorization.
    pub fn clear(&mut self, primary_name: &[u8], auth: &LockoutAuth) -> Result<(), LockoutError> {
        self.lockout_command(primary_name, &clear(auth))
    }

    /// One lockout command (`lockout_exchange`), then every handle it
    /// left owned flushed, best effort, each kept owned for drop if the
    /// TPM does not confirm: the primary, and a session the TPM did not
    /// end, a session whose StartAuthSession reply was refused after the
    /// TPM started it included. TPM2_Clear's success flushed the primary
    /// itself.
    pub(crate) fn lockout_command(
        &mut self,
        primary_name: &[u8],
        request: &Request<'_>,
    ) -> Result<(), LockoutError> {
        let owned = self.handles.clone();
        let result = self.lockout_exchange(primary_name, request);
        let cleared = request.code == CLEAR && result.is_ok();
        let made: Vec<u32> = self
            .handles
            .iter()
            .copied()
            .filter(|handle| !owned.contains(handle))
            .collect();
        for handle in made.into_iter().rev() {
            if cleared && handle >> 24 == 0x80 {
                self.handles.retain(|owned| *owned != handle);
            } else {
                let _ = self.flush(handle);
            }
        }
        result
    }

    /// One lockout command in an HMAC session salted to the storage
    /// primary, after refusing a primary with another Name.
    fn lockout_exchange(
        &mut self,
        primary_name: &[u8],
        request: &Request<'_>,
    ) -> Result<(), LockoutError> {
        let code = request.code;
        let failed =
            |client: &Self, message| LockoutError::from_refusal(code, client.refused, message);
        let primary = self
            .primary(None)
            .map_err(|message| failed(self, message))?;
        if primary.name != primary_name {
            return Err(LockoutError::OtherPrimary);
        }
        let mut session = self
            .salted_session(primary.handle, &primary.x, &primary.y, HMAC_SESSION)
            .map_err(|message| failed(self, message))?;
        self.call_session_changing(
            code,
            &[LOCKOUT],
            &[&LOCKOUT.to_be_bytes()],
            &mut session,
            request.auth,
            request.response_auth,
            request.attributes,
            &request.parameters,
        )
        .and_then(|out| Reader(&out).end())
        .map_err(|message| failed(self, message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{GET_CAPABILITY, PT_LOCKOUT_COUNTER, PT_PERMANENT, RC_AUTH_FAIL, RC_LOCKOUT};
    use crate::session::tests::{response, session, vector};
    use crate::session::{session_command, session_response};
    use crate::{CREATE_PRIMARY, FLUSH_CONTEXT, NO_SESSIONS, SESSIONS, START_AUTH_SESSION};

    fn lockout_auth() -> LockoutAuth {
        let mut bytes: [u8; 32] = vector("lockout_auth").try_into().unwrap();
        LockoutAuth::from_bytes(&mut bytes).unwrap()
    }

    /// Each command's whole bytes in the salted HMAC session, and its
    /// reply verified under the authorization the hierarchy then holds and
    /// refused under any other.
    #[test]
    fn lockout_commands_and_replies_match_the_vectors() {
        let auth = lockout_auth();
        let other = {
            let mut bytes = *auth.bytes();
            bytes[0] ^= 1;
            LockoutAuth::from_bytes(&mut bytes).unwrap()
        };
        let values = DaParameters {
            max_tries: 32,
            recovery_time: 600,
            lockout_recovery: 86400,
        };
        for (label, request, wrong) in [
            ("change_auth", change_auth(&auth).unwrap(), &[][..]),
            ("set_primary_policy", empty_policy(&auth), &[]),
            ("da_parameters", parameters(&auth, values), &[]),
            ("lock_reset", lock_reset(&auth), &[]),
            ("clear", clear(&auth), auth.bytes()),
        ] {
            let caller: [u8; 32] = vector(&format!("{label}_caller")).try_into().unwrap();
            let mut session = session("hmac_key", "hmac_nonce_tpm");
            let command = session_command(
                request.code,
                &[LOCKOUT],
                &[&LOCKOUT.to_be_bytes()],
                &session,
                request.auth,
                request.attributes,
                &request.parameters,
                &caller,
            )
            .unwrap();
            assert_eq!(command, vector(label), "{label}");
            let reply = vector(&format!("{label}_reply"));
            for refused in [wrong, other.bytes()] {
                assert!(
                    session_response(
                        request.code,
                        &mut session,
                        refused,
                        request.attributes,
                        &caller,
                        &reply
                    )
                    .is_err(),
                    "{label}"
                );
            }
            assert_eq!(
                session_response(
                    request.code,
                    &mut session,
                    request.response_auth,
                    request.attributes,
                    &caller,
                    &reply
                )
                .unwrap(),
                Vec::<u8>::new(),
                "{label}"
            );
        }
        // The new authorization crosses encrypted: its trimmed value is
        // not in the command.
        let trimmed = trim_auth(auth.bytes());
        assert_eq!(trimmed.len(), 31);
        assert!(!vector("change_auth")
            .windows(trimmed.len())
            .any(|window| window == trimmed));
        assert_eq!(change_auth(&auth).unwrap().parameters[..2], [0, 31]);
        // An empty authPolicy with TPM_ALG_NULL.
        assert_eq!(empty_policy(&auth).parameters, [0, 0, 0, 0x10]);
    }

    #[test]
    fn a_lockout_authorization_is_never_empty_and_zeroes_its_source() {
        let mut bytes = [0; 32];
        assert!(LockoutAuth::from_bytes(&mut bytes).is_err());
        let mut bytes = [9; 32];
        bytes[31] = 0;
        let auth = LockoutAuth::from_bytes(&mut bytes).unwrap();
        assert_eq!(bytes, [0; 32]);
        assert_eq!(auth.bytes()[..31], [9; 31]);
        assert_eq!(format!("{auth:?}"), "LockoutAuth(..)");
        let generated = LockoutAuth::generate().unwrap();
        assert_ne!(generated.bytes(), LockoutAuth::generate().unwrap().bytes());
    }

    /// A scripted TPM for the lockout commands: GetCapability reporting
    /// lockoutAuthSet as `set`, the vector primary, a salted HMAC session,
    /// and the lockout command answered with `reply`. It records each
    /// command code and owns nothing it flushes.
    struct Scripted {
        set: bool,
        reply: Vec<u8>,
        codes: Vec<u32>,
    }
    impl Transport for Scripted {
        fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
            let code = u32::from_be_bytes(command[6..10].try_into().unwrap());
            self.codes.push(code);
            let mut body = Vec::new();
            match code {
                GET_CAPABILITY => {
                    let first = u32::from_be_bytes(command[14..18].try_into().unwrap());
                    let count = u32::from_be_bytes(command[18..22].try_into().unwrap());
                    body.push(0);
                    put32(&mut body, 6);
                    put32(&mut body, count);
                    for offset in 0..count {
                        put32(&mut body, first + offset);
                        let value = if first == PT_PERMANENT { 4 } else { 0 };
                        put32(&mut body, if self.set { value } else { 0 });
                    }
                    assert!(first == PT_PERMANENT || first == PT_LOCKOUT_COUNTER);
                }
                CREATE_PRIMARY => {
                    let mut parameters = Vec::new();
                    put_blob(&mut parameters, &vector("primary_public")).unwrap();
                    put_blob(&mut parameters, &[]).unwrap();
                    put_blob(&mut parameters, &[4; 32]).unwrap();
                    crate::put16(&mut parameters, 0x8021);
                    put32(&mut parameters, crate::OWNER);
                    put_blob(&mut parameters, &[5; 32]).unwrap();
                    put_blob(&mut parameters, &vector("primary_name")).unwrap();
                    put32(&mut body, 0x8000_0000);
                    put32(&mut body, parameters.len() as u32);
                    body.extend_from_slice(&parameters);
                    body.extend_from_slice(&[0, 0, 1, 0, 0]);
                    return Ok(response(SESSIONS, 0, &body));
                }
                START_AUTH_SESSION => {
                    put32(&mut body, 0x0200_0000);
                    put_blob(&mut body, &[0x0e; 32]).unwrap();
                }
                FLUSH_CONTEXT => {}
                _ => return Ok(self.reply.clone()),
            }
            Ok(response(NO_SESSIONS, 0, &body))
        }
    }

    fn scripted(set: bool, reply: Vec<u8>) -> Client<Scripted> {
        Client::new(Scripted {
            set,
            reply,
            codes: Vec::new(),
        })
    }

    #[test]
    fn take_refuses_a_set_lockout_authorization_before_any_session() {
        let auth = lockout_auth();
        let name = vector("primary_name");
        let mut client = scripted(true, Vec::new());
        assert_eq!(
            client.take_lockout(&name, &auth),
            Err(LockoutError::AlreadySet)
        );
        assert_eq!(client.transport.codes, [GET_CAPABILITY, GET_CAPABILITY]);
        // Clear, the take goes on to HierarchyChangeAuth, and a refused
        // change sends no SetPrimaryPolicy.
        let mut client = scripted(false, response(NO_SESSIONS, RC_AUTH_FAIL, &[]));
        let refused = client.take_lockout(&name, &auth).unwrap_err();
        assert_eq!(refused.authorization(), Some(AuthRefusal::AuthFail));
        assert_eq!(
            client.transport.codes,
            [
                GET_CAPABILITY,
                GET_CAPABILITY,
                CREATE_PRIMARY,
                START_AUTH_SESSION,
                HIERARCHY_CHANGE_AUTH,
                FLUSH_CONTEXT,
                FLUSH_CONTEXT
            ]
        );
        assert_eq!(client.owned_handles(), 0);
    }

    #[test]
    fn lockout_authorization_refusals_are_typed_and_sent_once() {
        let auth = lockout_auth();
        let name = vector("primary_name");
        let values = DaParameters {
            max_tries: 32,
            recovery_time: 600,
            lockout_recovery: 86400,
        };
        type Command = fn(&mut Client<Scripted>, &[u8], &LockoutAuth) -> Result<(), LockoutError>;
        let commands: &[(u32, Command)] = &[
            (SET_PRIMARY_POLICY, |client, name, auth| {
                client.clear_lockout_policy(name, auth)
            }),
            (DICTIONARY_ATTACK_PARAMETERS, |client, name, auth| {
                client.set_dictionary_attack(
                    name,
                    auth,
                    DaParameters {
                        max_tries: 32,
                        recovery_time: 600,
                        lockout_recovery: 86400,
                    },
                )
            }),
            (DICTIONARY_ATTACK_LOCK_RESET, |client, name, auth| {
                client.reset_lockout(name, auth)
            }),
            (CLEAR, |client, name, auth| client.clear(name, auth)),
        ];
        assert_eq!(values, parameters(&auth, values).values());
        for (code, command) in commands {
            for (rc, kind) in [
                (RC_AUTH_FAIL, AuthRefusal::AuthFail),
                (RC_LOCKOUT, AuthRefusal::Lockout),
            ] {
                let mut client = scripted(false, response(NO_SESSIONS, rc, &[]));
                let refused = command(&mut client, &name, &auth).unwrap_err();
                assert_eq!(
                    refused,
                    LockoutError::Authorization {
                        command: *code,
                        kind
                    }
                );
                assert_eq!(
                    client.transport.codes,
                    [
                        CREATE_PRIMARY,
                        START_AUTH_SESSION,
                        *code,
                        FLUSH_CONTEXT,
                        FLUSH_CONTEXT
                    ],
                    "one attempt, no retry"
                );
                assert_eq!(client.owned_handles(), 0);
            }
            // Another refusal of the command is not an authorization one.
            let mut client = scripted(false, response(NO_SESSIONS, 0x9a2, &[]));
            let refused = command(&mut client, &name, &auth).unwrap_err();
            assert_eq!(refused.authorization(), None);
            assert!(matches!(
                refused,
                LockoutError::Failed {
                    refusal: Some(Refusal { rc: 0x9a2, .. }),
                    ..
                }
            ));
            // A well-formed success whose HMAC does not verify is not a
            // success, nor a typed refusal; its session and the primary
            // are flushed.
            let mut forged = vec![0; 4];
            put_blob(&mut forged, &[0x6e; 32]).unwrap();
            forged.push(0);
            put_blob(&mut forged, &[0x55; 32]).unwrap();
            let mut client = scripted(false, response(SESSIONS, 0, &forged));
            let refused = command(&mut client, &name, &auth).unwrap_err();
            assert_eq!(
                refused,
                LockoutError::Failed {
                    refusal: None,
                    message: "TPM response HMAC does not verify".into()
                }
            );
            assert_eq!(
                client.transport.codes,
                [
                    CREATE_PRIMARY,
                    START_AUTH_SESSION,
                    *code,
                    FLUSH_CONTEXT,
                    FLUSH_CONTEXT
                ]
            );
            assert_eq!(client.owned_handles(), 0);
            // Another primary is refused before any session is salted.
            let mut other = name.clone();
            other[2] ^= 1;
            let mut client = scripted(false, Vec::new());
            assert_eq!(
                command(&mut client, &other, &auth),
                Err(LockoutError::OtherPrimary)
            );
            assert_eq!(client.transport.codes, [CREATE_PRIMARY, FLUSH_CONTEXT]);
        }
        // TPM_RC_DISABLED is Clear's own refusal, typed; from another
        // command it is not.
        let mut client = scripted(false, response(NO_SESSIONS, RC_DISABLED, &[]));
        assert_eq!(client.clear(&name, &auth), Err(LockoutError::ClearDisabled));
        assert_eq!(client.owned_handles(), 0);
        let mut client = scripted(false, response(NO_SESSIONS, RC_DISABLED, &[]));
        assert!(matches!(
            client.reset_lockout(&name, &auth),
            Err(LockoutError::Failed { .. })
        ));
        // A refusal of a command before the lockout one, with the same
        // response code, is not typed as the lockout authorization's.
        let refused = LockoutError::from_refusal(
            CLEAR,
            Some(Refusal {
                command: CREATE_PRIMARY,
                rc: RC_AUTH_FAIL,
            }),
            String::new(),
        );
        assert_eq!(refused.authorization(), None);
    }

    impl Request<'_> {
        fn values(&self) -> DaParameters {
            let word =
                |at: usize| u32::from_be_bytes(self.parameters[at..at + 4].try_into().unwrap());
            DaParameters {
                max_tries: word(0),
                recovery_time: word(4),
                lockout_recovery: word(8),
            }
        }
    }
}
