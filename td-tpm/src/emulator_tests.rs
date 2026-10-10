//! The pinned-swtpm oracle for salted and PIN-authorized sessions and the
//! lockout hierarchy, run as td-secret/DESIGN.md "TPM validation" runs
//! td-secret's: `TD_TEST_SWTPM=/absolute/path/to/swtpm cargo test
//! --manifest-path td-tpm/Cargo.toml emulator_ -- --ignored`.

use super::*;
use std::cell::RefCell;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::lockout::{change_auth, Request};

const SHUTDOWN: u32 = 0x145;
const CLEAR_CONTROL: u32 = 0x127;
/// TPM_RC_AUTH_UNAVAILABLE: a policy session for an empty authPolicy.
const RC_AUTH_UNAVAILABLE: u32 = 0x12f;
/// td-install/ENCRYPTION.md's dictionary-attack parameters.
const TD_PARAMETERS: DaParameters = DaParameters {
    max_tries: 32,
    recovery_time: 600,
    lockout_recovery: 86400,
};

/// A socket transport recording every command code it carries.
struct Socket {
    stream: UnixStream,
    codes: Rc<RefCell<Vec<u32>>>,
}
impl Transport for Socket {
    fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
        if let Some(code) = command.get(6..10) {
            self.codes
                .borrow_mut()
                .push(u32::from_be_bytes(code.try_into().unwrap()));
        }
        self.stream.write_all(command).map_err(|e| e.to_string())?;
        let mut header = [0; 10];
        self.stream
            .read_exact(&mut header)
            .map_err(|e| e.to_string())?;
        let size = u32::from_be_bytes(header[2..6].try_into().unwrap()) as usize;
        if !(10..=MAX_PACKET).contains(&size) {
            return Err("invalid test TPM packet".into());
        }
        let mut reply = header.to_vec();
        reply.resize(size, 0);
        self.stream
            .read_exact(&mut reply[10..])
            .map_err(|e| e.to_string())?;
        Ok(reply)
    }
}

struct Emulator {
    child: Child,
    state: PathBuf,
    socket: PathBuf,
    codes: Rc<RefCell<Vec<u32>>>,
}

fn spawn(state: &Path, socket: &Path) -> Child {
    let executable =
        std::env::var_os("TD_TEST_SWTPM").expect("set TD_TEST_SWTPM to pinned swtpm 0.10.1");
    let version = Command::new(&executable).arg("--version").output().unwrap();
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("TPM emulator version 0.10.1,"));
    let mut child = Command::new(executable)
        .args(["socket", "--tpm2", "--tpmstate"])
        .arg(format!("dir={},mode=0600", state.display()))
        .arg("--server")
        .arg(format!("type=unixio,path={}", socket.display()))
        .args(["--flags", "not-need-init,startup-clear"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    // The socket path appears at bind(), before swtpm listens: wait
    // for a connection it accepts, not for the path.
    while std::os::unix::net::UnixStream::connect(socket).is_err() {
        assert!(child.try_wait().unwrap().is_none(), "swtpm exited");
        assert!(Instant::now() < deadline, "swtpm startup timeout");
        std::thread::sleep(Duration::from_millis(10));
    }
    child
}

impl Emulator {
    fn start(name: &str) -> Self {
        let state = std::env::temp_dir().join(format!(
            "td-tpm-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&state).unwrap();
        let socket = state.join("socket");
        Self {
            child: spawn(&state, &socket),
            state,
            socket,
            codes: Rc::default(),
        }
    }

    fn client(&self) -> Client<Socket> {
        let stream = UnixStream::connect(&self.socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Client::new(Socket {
            stream,
            codes: self.codes.clone(),
        })
    }

    /// The command codes sent since the last call.
    fn codes(&self) -> Vec<u32> {
        std::mem::take(&mut self.codes.borrow_mut())
    }

    /// An orderly TPM2_Shutdown(CLEAR), then a new swtpm process on the
    /// same state: a reboot, its PCRs at reset.
    fn restart(&mut self) {
        let (_, out) = self
            .client()
            .call(SHUTDOWN, &[], None, &[0, 0], false)
            .unwrap();
        assert!(out.is_empty());
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
        self.child = spawn(&self.state, &self.socket);
    }

    /// The fixture selector's measurements in PCRs 4 and 9.
    fn measure(&self) {
        let mut client = self.client();
        client.extend_pcr(4, &[0x44; 32]).unwrap();
        client.extend_pcr(9, &[0x49; 32]).unwrap();
    }

    /// The tpm-pin shape: PCRs 4 and 9 as they read now, and a literal-zero
    /// PCR 12.
    fn policy(&self) -> PcrPolicy {
        let mut values = self
            .client()
            .read_pcrs(PcrSelection::new(1 << 4 | 1 << 9).unwrap())
            .unwrap();
        values.push([0; 32]);
        PcrPolicy {
            selection: PcrSelection::new(1 << 4 | 1 << 9 | 1 << 12).unwrap(),
            pcr_digest: pcr_digest(&values),
        }
    }

    /// A policy over every selected PCR as it reads now, PCR 12 included:
    /// it passes PolicyPCR, so only Unseal's policy check refuses it.
    fn current_policy(&self) -> PcrPolicy {
        let selection = PcrSelection::new(1 << 4 | 1 << 9 | 1 << 12).unwrap();
        PcrPolicy {
            selection,
            pcr_digest: pcr_digest(&self.client().read_pcrs(selection).unwrap()),
        }
    }

    fn counter(&self) -> u32 {
        self.client().dictionary_attack().unwrap().counter
    }

    /// The unpersonalized storage primary's Name, as a token records it.
    fn primary_name(&self) -> Vec<u8> {
        self.client().storage_primary(None).unwrap().1
    }

    /// Take the lockout hierarchy with a fresh authorization and set
    /// `values` under it, as td's take does.
    fn take(&self, values: DaParameters) -> LockoutAuth {
        let auth = LockoutAuth::generate().unwrap();
        let name = self.primary_name();
        let mut client = self.client();
        client.take_lockout(&name, &auth).unwrap();
        client.set_dictionary_attack(&name, &auth, values).unwrap();
        assert_eq!(client.owned_handles(), 0);
        auth
    }

    /// Another system's lockoutPolicy, set under the empty lockoutAuth:
    /// the zero digest of a policy session with no assertions.
    fn foreign_lockout_policy(&self) {
        let mut parameters = Vec::new();
        put_blob(&mut parameters, &[0; 32]).unwrap();
        put16(&mut parameters, SHA256);
        self.client()
            .call(
                SET_PRIMARY_POLICY,
                &[LOCKOUT],
                Some(PASSWORD),
                &parameters,
                false,
            )
            .unwrap();
    }

    /// `code` authorizing TPM_RH_LOCKOUT by a fresh unsalted policy session
    /// with no assertions, as that foreign policy admits; its refusal.
    fn policy_command(&self, code: u32, parameters: &[u8]) -> Result<(), Option<Refusal>> {
        let mut client = self.client();
        let session = client.start_session(POLICY_SESSION).unwrap();
        client
            .call(code, &[LOCKOUT], Some(session), parameters, false)
            .map(|_| ())
            .map_err(|_| client.last_refusal())
    }

    fn parameters(&self) -> DaParameters {
        let state = self.client().dictionary_attack().unwrap();
        DaParameters {
            max_tries: state.max_tries,
            recovery_time: state.interval,
            lockout_recovery: state.recovery,
        }
    }
}

/// An authorization other than `auth`.
fn wrong(auth: &LockoutAuth) -> LockoutAuth {
    let mut bytes = *auth.bytes();
    bytes[0] ^= 1;
    bytes[1] |= 1;
    LockoutAuth::from_bytes(&mut bytes).unwrap()
}

impl Drop for Emulator {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.state);
    }
}

/// A PIN-derived authValue with a trailing zero the TPM trims.
fn pin(byte: u8) -> [u8; 32] {
    let mut auth = [byte; 32];
    auth[31] = 0;
    auth
}

const PAYLOAD: [u8; 32] = [0x5a; 32];

fn seal(tpm: &Emulator, policy: &PcrPolicy, attributes: u32) -> AuthSealed {
    let mut payload = PAYLOAD;
    let sealed = tpm
        .client()
        .seal_with_auth(policy, None, &pin(7), attributes, &mut payload)
        .unwrap();
    assert_eq!(payload, [0; 32]);
    sealed
}

fn unseal(
    tpm: &Emulator,
    policy: &PcrPolicy,
    sealed: &AuthSealed,
    auth: &[u8],
) -> Result<Vec<u8>, UnsealError> {
    tpm.client().unseal_with_auth(
        policy,
        &sealed.primary_name,
        auth,
        &sealed.object.public,
        &sealed.object.private,
    )
}

#[test]
#[ignore = "needs TD_TEST_SWTPM, the pinned swtpm 0.10.1"]
fn emulator_seals_and_unseals_with_the_right_pin_in_a_salted_session() {
    let tpm = Emulator::start("pin");
    tpm.measure();
    let policy = tpm.policy();
    let sealed = seal(&tpm, &policy, DA_SEALED_ATTRIBUTES);
    assert_eq!(
        validate_auth_sealed_public(&sealed.object.public, &policy.auth_digest()),
        Ok(DA_SEALED_ATTRIBUTES)
    );
    let (primary, name) = tpm.client().storage_primary(None).unwrap();
    assert_eq!(name, sealed.primary_name);
    assert!(primary >> 24 == 0x80);
    tpm.codes();
    assert_eq!(unseal(&tpm, &policy, &sealed, &pin(7)).unwrap(), PAYLOAD);
    assert!(tpm.codes().contains(&UNSEAL));
    // The TPM trimmed the authValue's trailing zero; the client trims too.
    assert_eq!(
        unseal(&tpm, &policy, &sealed, trim_auth(&pin(7))).unwrap(),
        PAYLOAD
    );
    // The PCR-only path refuses a PIN object by its public area alone,
    // before Load: it never reaches the TPM's policy check.
    tpm.codes();
    assert!(tpm
        .client()
        .unseal_object(&policy, None, &sealed.object.public, &sealed.object.private)
        .is_err());
    assert!(!tpm.codes().contains(&LOAD));
    assert_eq!(tpm.counter(), 0);
}

#[test]
#[ignore = "needs TD_TEST_SWTPM, the pinned swtpm 0.10.1"]
fn emulator_counts_a_wrong_pin_across_a_restart() {
    let mut tpm = Emulator::start("wrong");
    tpm.measure();
    let policy = tpm.policy();
    let sealed = seal(&tpm, &policy, DA_SEALED_ATTRIBUTES);
    let before = tpm.client().dictionary_attack().unwrap();
    assert_eq!(before.counter, 0);
    assert!(!before.in_lockout && !before.lockout_auth_set);
    let refused = unseal(&tpm, &policy, &sealed, &pin(8)).unwrap_err();
    assert_eq!(
        refused.refusal,
        Some(Refusal {
            command: UNSEAL,
            rc: RC_AUTH_FAIL
        })
    );
    assert_eq!(refused.authorization(), Some(AuthRefusal::AuthFail));
    assert_eq!(tpm.counter(), 1);
    tpm.restart();
    assert_eq!(tpm.counter(), 1);
    tpm.measure();
    assert_eq!(unseal(&tpm, &policy, &sealed, &pin(7)).unwrap(), PAYLOAD);
    assert_eq!(tpm.counter(), 1);

    // With noDA set the TPM refuses the wrong PIN as TPM_RC_BAD_AUTH,
    // without dictionary-attack implications, and counts nothing.
    let exempt = seal(&tpm, &policy, SEALED_ATTRIBUTES);
    let refused = unseal(&tpm, &policy, &exempt, &pin(8)).unwrap_err();
    assert_eq!(
        refused.refusal,
        Some(Refusal {
            command: UNSEAL,
            rc: 0x9a2
        })
    );
    assert_eq!(refused.authorization(), None);
    assert_eq!(tpm.counter(), 1);
    assert_eq!(unseal(&tpm, &policy, &exempt, &pin(7)).unwrap(), PAYLOAD);
}

#[test]
#[ignore = "needs TD_TEST_SWTPM, the pinned swtpm 0.10.1"]
fn emulator_locks_out_after_32_wrong_pins_until_the_lockout_reset() {
    let mut tpm = Emulator::start("lockout");
    let auth = tpm.take(TD_PARAMETERS);
    assert_eq!(tpm.parameters(), TD_PARAMETERS);
    tpm.measure();
    let policy = tpm.policy();
    let sealed = seal(&tpm, &policy, DA_SEALED_ATTRIBUTES);
    for attempt in 1..=32 {
        let refused = unseal(&tpm, &policy, &sealed, &pin(8)).unwrap_err();
        assert_eq!(refused.authorization(), Some(AuthRefusal::AuthFail));
        assert_eq!(tpm.counter(), attempt);
    }
    assert!(tpm.client().dictionary_attack().unwrap().in_lockout);
    for auth in [pin(8), pin(7)] {
        let refused = unseal(&tpm, &policy, &sealed, &auth).unwrap_err();
        assert_eq!(
            refused.refusal,
            Some(Refusal {
                command: UNSEAL,
                rc: RC_LOCKOUT
            })
        );
        assert_eq!(refused.authorization(), Some(AuthRefusal::Lockout));
    }
    // The lockout outlasts a restart; the reset under the lockout
    // authorization ends it, and the right PIN unseals again.
    tpm.restart();
    tpm.measure();
    let state = tpm.client().dictionary_attack().unwrap();
    assert!(state.in_lockout && state.lockout_auth_set);
    assert_eq!(state.counter, 32);
    tpm.client()
        .reset_lockout(&sealed.primary_name, &auth)
        .unwrap();
    let state = tpm.client().dictionary_attack().unwrap();
    assert!(!state.in_lockout);
    assert_eq!(state.counter, 0);
    assert_eq!(unseal(&tpm, &policy, &sealed, &pin(7)).unwrap(), PAYLOAD);
}

#[test]
#[ignore = "needs TD_TEST_SWTPM, the pinned swtpm 0.10.1"]
fn emulator_takes_the_lockout_hierarchy_once_and_keeps_it_across_a_restart() {
    let mut tpm = Emulator::start("take");
    let name = tpm.primary_name();
    let auth = LockoutAuth::generate().unwrap();
    assert!(!tpm.client().dictionary_attack().unwrap().lockout_auth_set);
    // Under another primary nothing reaches the lockout hierarchy.
    let mut other = name.clone();
    other[2] ^= 1;
    tpm.codes();
    assert_eq!(
        tpm.client().take_lockout(&other, &auth),
        Err(LockoutError::OtherPrimary)
    );
    let codes = tpm.codes();
    assert!(!codes.contains(&START_AUTH_SESSION) && !codes.contains(&HIERARCHY_CHANGE_AUTH));
    assert!(!tpm.client().dictionary_attack().unwrap().lockout_auth_set);

    let mut client = tpm.client();
    client.take_lockout(&name, &auth).unwrap();
    assert_eq!(client.owned_handles(), 0);
    drop(client);
    assert!(tpm.client().dictionary_attack().unwrap().lockout_auth_set);
    // A second take, under any value, is refused before the lockout
    // hierarchy sees it: no guess is spent.
    tpm.codes();
    for value in [&auth, &LockoutAuth::generate().unwrap()] {
        assert_eq!(
            tpm.client().take_lockout(&name, value),
            Err(LockoutError::AlreadySet)
        );
    }
    assert!(tpm.codes().iter().all(|code| *code == GET_CAPABILITY));
    // The authorization taken proves itself; it and the parameters set
    // under it outlast a restart.
    tpm.client().reset_lockout(&name, &auth).unwrap();
    tpm.client()
        .set_dictionary_attack(&name, &auth, TD_PARAMETERS)
        .unwrap();
    tpm.restart();
    assert!(tpm.client().dictionary_attack().unwrap().lockout_auth_set);
    assert_eq!(tpm.parameters(), TD_PARAMETERS);
    assert_eq!(
        tpm.client().take_lockout(&name, &auth),
        Err(LockoutError::AlreadySet)
    );
    tpm.client().reset_lockout(&name, &auth).unwrap();
}

#[test]
#[ignore = "needs TD_TEST_SWTPM, the pinned swtpm 0.10.1"]
fn emulator_sets_the_dictionary_attack_parameters_under_the_lockout_authorization() {
    let tpm = Emulator::start("parameters");
    let auth = tpm.take(TD_PARAMETERS);
    assert_eq!(tpm.parameters(), TD_PARAMETERS);
    let name = tpm.primary_name();
    let other = DaParameters {
        max_tries: 3,
        recovery_time: 30,
        lockout_recovery: 60,
    };
    tpm.client()
        .set_dictionary_attack(&name, &auth, other)
        .unwrap();
    assert_eq!(tpm.parameters(), other);
    // Three wrong PINs now lock the TPM out.
    tpm.measure();
    let policy = tpm.policy();
    let sealed = seal(&tpm, &policy, DA_SEALED_ATTRIBUTES);
    for _ in 0..3 {
        let refused = unseal(&tpm, &policy, &sealed, &pin(8)).unwrap_err();
        assert_eq!(refused.authorization(), Some(AuthRefusal::AuthFail));
    }
    let refused = unseal(&tpm, &policy, &sealed, &pin(7)).unwrap_err();
    assert_eq!(refused.authorization(), Some(AuthRefusal::Lockout));
    // td's values repair them, under the same authorization.
    tpm.client()
        .set_dictionary_attack(&name, &auth, TD_PARAMETERS)
        .unwrap();
    assert_eq!(tpm.parameters(), TD_PARAMETERS);
}

/// DictionaryAttackParameters' bytes for a policy-session attempt.
fn da_bytes(values: DaParameters) -> Vec<u8> {
    let mut bytes = Vec::new();
    for value in [
        values.max_tries,
        values.recovery_time,
        values.lockout_recovery,
    ] {
        put32(&mut bytes, value);
    }
    bytes
}

#[test]
#[ignore = "needs TD_TEST_SWTPM, the pinned swtpm 0.10.1"]
fn emulator_takeover_empties_a_lockout_policy_another_system_set() {
    let tpm = Emulator::start("policy");
    tpm.foreign_lockout_policy();
    let foreign = DaParameters {
        max_tries: 1000,
        recovery_time: 1,
        lockout_recovery: 1,
    };
    // Before the take the foreign policy authorizes the hierarchy, with
    // lockoutAuthSet still clear.
    tpm.policy_command(DICTIONARY_ATTACK_LOCK_RESET, &[])
        .unwrap();
    tpm.policy_command(DICTIONARY_ATTACK_PARAMETERS, &da_bytes(foreign))
        .unwrap();
    assert_eq!(tpm.parameters(), foreign);
    assert!(!tpm.client().dictionary_attack().unwrap().lockout_auth_set);

    tpm.codes();
    let auth = tpm.take(TD_PARAMETERS);
    let codes = tpm.codes();
    let change = codes.iter().position(|code| *code == HIERARCHY_CHANGE_AUTH);
    let policy = codes.iter().position(|code| *code == SET_PRIMARY_POLICY);
    assert!(change.is_some() && change < policy, "{codes:x?}");
    // After it no policy session authorizes the hierarchy, and the
    // refusals cost the lockout authorization nothing.
    for (code, parameters) in [
        (DICTIONARY_ATTACK_LOCK_RESET, Vec::new()),
        (DICTIONARY_ATTACK_PARAMETERS, da_bytes(foreign)),
    ] {
        let refused = tpm.policy_command(code, &parameters).unwrap_err();
        assert_eq!(
            refused,
            Some(Refusal {
                command: code,
                rc: RC_AUTH_UNAVAILABLE
            })
        );
    }
    assert_eq!(tpm.parameters(), TD_PARAMETERS);
    let name = tpm.primary_name();
    tpm.client().reset_lockout(&name, &auth).unwrap();
}

#[test]
#[ignore = "needs TD_TEST_SWTPM, the pinned swtpm 0.10.1"]
fn emulator_a_proof_repairs_a_take_interrupted_before_its_policy() {
    let tpm = Emulator::start("repair");
    tpm.foreign_lockout_policy();
    let name = tpm.primary_name();
    let auth = LockoutAuth::generate().unwrap();
    // HierarchyChangeAuth alone, as a take that stopped before
    // SetPrimaryPolicy left it: the foreign policy still authorizes.
    tpm.client()
        .lockout_command(&name, &change_auth(&auth).unwrap())
        .unwrap();
    assert!(tpm.client().dictionary_attack().unwrap().lockout_auth_set);
    tpm.policy_command(DICTIONARY_ATTACK_LOCK_RESET, &[])
        .unwrap();
    // The proof's SetPrimaryPolicy under the authorization ends that.
    tpm.client().clear_lockout_policy(&name, &auth).unwrap();
    assert!(tpm
        .policy_command(DICTIONARY_ATTACK_LOCK_RESET, &[])
        .is_err());
    tpm.client().clear_lockout_policy(&name, &auth).unwrap();
}

#[test]
#[ignore = "needs TD_TEST_SWTPM, the pinned swtpm 0.10.1"]
fn emulator_refuses_a_wrong_lockout_authorization_until_lockout_recovery() {
    let tpm = Emulator::start("wrong-lockout");
    // lockoutRecovery in seconds: the refusals below must land inside it.
    const RECOVERY: u32 = 4;
    let recovery = DaParameters {
        lockout_recovery: RECOVERY,
        ..TD_PARAMETERS
    };
    let auth = tpm.take(recovery);
    let name = tpm.primary_name();
    let wrong = wrong(&auth);
    let refused = |command: u32, kind| Err(LockoutError::Authorization { command, kind });
    type Command = fn(&mut Client<Socket>, &[u8], &LockoutAuth) -> Result<(), LockoutError>;
    let reset: Command = |client, name, auth| client.reset_lockout(name, auth);
    let prove: Command = |client, name, auth| {
        client.set_dictionary_attack(
            name,
            auth,
            DaParameters {
                lockout_recovery: RECOVERY,
                ..TD_PARAMETERS
            },
        )
    };
    for (code, command) in [
        (DICTIONARY_ATTACK_LOCK_RESET, reset),
        (DICTIONARY_ATTACK_PARAMETERS, prove),
    ] {
        // A wrong value is refused once, and then the right one too: the
        // lockout hierarchy allows one failure per lockoutRecovery.
        tpm.codes();
        assert_eq!(
            command(&mut tpm.client(), &name, &wrong),
            refused(code, AuthRefusal::AuthFail)
        );
        let failed_at = Instant::now();
        assert_eq!(tpm.codes().iter().filter(|sent| **sent == code).count(), 1);
        for command in [reset, prove] {
            let locked = command(&mut tpm.client(), &name, &auth).unwrap_err();
            assert_eq!(locked.authorization(), Some(AuthRefusal::Lockout));
        }
        // Both refusals fell inside lockoutRecovery, so the success below
        // is its end and not a slow run's.
        let window = Duration::from_secs(RECOVERY.into());
        assert!(failed_at.elapsed() < window, "{:?}", failed_at.elapsed());
        // A wrong value counts against the lockout hierarchy alone.
        let state = tpm.client().dictionary_attack().unwrap();
        assert!(!state.in_lockout);
        assert_eq!(state.counter, 0);
        std::thread::sleep((window + Duration::from_secs(1)).saturating_sub(failed_at.elapsed()));
        command(&mut tpm.client(), &name, &auth).unwrap();
    }
}

#[test]
#[ignore = "needs TD_TEST_SWTPM, the pinned swtpm 0.10.1"]
fn emulator_clears_the_tpm_under_the_lockout_authorization() {
    let tpm = Emulator::start("clear");
    tpm.measure();
    let policy = tpm.policy();
    let sealed = seal(&tpm, &policy, DA_SEALED_ATTRIBUTES);
    let auth = tpm.take(TD_PARAMETERS);
    let mut client = tpm.client();
    client.clear(&sealed.primary_name, &auth).unwrap();
    assert_eq!(client.owned_handles(), 0);
    drop(client);
    // The lockout authorization is empty again, the storage seed new, and
    // the object sealed under the old one no longer loads.
    assert!(!tpm.client().dictionary_attack().unwrap().lockout_auth_set);
    let name = tpm.primary_name();
    assert_ne!(name, sealed.primary_name);
    let refused = unseal(&tpm, &policy, &sealed, &pin(7)).unwrap_err();
    assert!(refused.message.contains("storage primary"));
    let mut recorded = sealed.clone();
    recorded.primary_name = name.clone();
    let refused = unseal(&tpm, &policy, &recorded, &pin(7)).unwrap_err();
    assert_eq!(
        refused.refusal,
        Some(Refusal {
            command: LOAD,
            rc: 0x1df
        })
    );
    // A cleared TPM can be taken again, under its new primary.
    assert_eq!(
        tpm.client().reset_lockout(&sealed.primary_name, &auth),
        Err(LockoutError::OtherPrimary)
    );
    let again = tpm.take(TD_PARAMETERS);
    tpm.client().reset_lockout(&name, &again).unwrap();
    // TPM2_ClearControl(YES) under it, as another system might send:
    // Clear is then the typed ClearDisabled, its TPM_RC_DISABLED.
    let disable = Request {
        code: CLEAR_CONTROL,
        auth: again.bytes(),
        response_auth: again.bytes(),
        attributes: 0,
        parameters: vec![1],
    };
    tpm.client().lockout_command(&name, &disable).unwrap();
    let mut client = tpm.client();
    assert_eq!(
        client.clear(&name, &again),
        Err(LockoutError::ClearDisabled)
    );
    assert_eq!(client.last_refusal(), None, "the flushes ran after it");
    drop(client);
    assert!(tpm.client().dictionary_attack().unwrap().lockout_auth_set);
}

#[test]
#[ignore = "needs TD_TEST_SWTPM, the pinned swtpm 0.10.1"]
fn emulator_refuses_a_changed_chain_and_a_closed_cap_without_counting() {
    let mut tpm = Emulator::start("chain");
    tpm.measure();
    let policy = tpm.policy();
    let sealed = seal(&tpm, &policy, DA_SEALED_ATTRIBUTES);
    let policy_pcr = Some(Refusal {
        command: POLICY_PCR,
        rc: 0x1c4,
    });
    let policy_fail = Some(Refusal {
        command: UNSEAL,
        rc: 0x99d,
    });
    for change in [4, 12] {
        tpm.restart();
        tpm.measure();
        tpm.client().extend_pcr(change, &[0x5c; 32]).unwrap();
        tpm.codes();
        // The sealed policy's own PolicyPCR is refused; no Unseal is sent.
        for auth in [pin(7), pin(8)] {
            let refused = unseal(&tpm, &policy, &sealed, &auth).unwrap_err();
            assert_eq!(refused.refusal, policy_pcr, "PCR {change}");
        }
        assert!(!tpm.codes().contains(&UNSEAL));
        // A session over the PCRs as they read now reaches Unseal, which
        // compares the policy with the object's before the HMAC: a wrong
        // PIN is POLICY_FAIL and costs no attempt.
        for auth in [pin(7), pin(8)] {
            let refused = unseal(&tpm, &tpm.current_policy(), &sealed, &auth).unwrap_err();
            assert_eq!(refused.refusal, policy_fail, "PCR {change}");
        }
        assert_eq!(tpm.counter(), 0, "PCR {change}");
    }
}

#[test]
#[ignore = "needs TD_TEST_SWTPM, the pinned swtpm 0.10.1"]
fn emulator_refuses_a_fresh_tpm_and_another_primary_before_seal_or_unseal() {
    let first = Emulator::start("first");
    first.measure();
    let policy = first.policy();
    let sealed = seal(&first, &policy, DA_SEALED_ATTRIBUTES);
    // A re-seal that records this primary is sealed under it.
    let mut payload = PAYLOAD;
    let resealed = first
        .client()
        .seal_with_auth(
            &policy,
            Some(&sealed.primary_name),
            &pin(8),
            DA_SEALED_ATTRIBUTES,
            &mut payload,
        )
        .unwrap();
    assert_eq!(resealed.primary_name, sealed.primary_name);
    assert_eq!(
        unseal(&first, &policy, &resealed, &pin(8)).unwrap(),
        PAYLOAD
    );

    // Another primary's Name is refused before Load and Unseal.
    first.codes();
    let mut other = sealed.clone();
    other.primary_name[2] ^= 1;
    let refused = unseal(&first, &policy, &other, &pin(7)).unwrap_err();
    assert_eq!(refused.refusal, None);
    assert!(refused.message.contains("storage primary"));
    let codes = first.codes();
    assert!(!codes.contains(&LOAD) && !codes.contains(&UNSEAL));
    assert_eq!(first.counter(), 0);

    // A fresh TPM state has another primary, and its seed does not load
    // the object even under its own primary's Name.
    let fresh = Emulator::start("fresh");
    fresh.measure();
    // A re-seal that records the first primary is refused under the
    // fresh one before its session is salted: no Create is sent.
    fresh.codes();
    let mut payload = PAYLOAD;
    let refused = fresh
        .client()
        .seal_with_auth(
            &policy,
            Some(&sealed.primary_name),
            &pin(8),
            DA_SEALED_ATTRIBUTES,
            &mut payload,
        )
        .unwrap_err();
    assert!(refused.contains("storage primary"));
    assert_eq!(payload, [0; 32]);
    let codes = fresh.codes();
    assert_eq!(
        codes
            .iter()
            .filter(|code| **code == START_AUTH_SESSION)
            .count(),
        1,
        "only the trial session"
    );
    assert!(!codes.contains(&CREATE));
    let refused = unseal(&fresh, &policy, &sealed, &pin(7)).unwrap_err();
    assert_eq!(refused.refusal, None);
    let (_, name) = fresh.client().storage_primary(None).unwrap();
    assert_ne!(name, sealed.primary_name);
    let integrity = Some(Refusal {
        command: LOAD,
        rc: 0x1df,
    });
    let mut recorded = sealed.clone();
    recorded.primary_name = name;
    let refused = unseal(&fresh, &policy, &recorded, &pin(7)).unwrap_err();
    assert_eq!(refused.refusal, integrity);
    assert_eq!(
        fresh
            .client()
            .load_and_flush(None, &sealed.object.public, &sealed.object.private)
            .unwrap_err(),
        "TPM command 0x157 refused: 0x1df"
    );
    assert_eq!(fresh.counter(), 0);
}
