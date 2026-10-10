//! The pinned-swtpm oracle for salted and PIN-authorized sessions, run as
//! td-secret/DESIGN.md "TPM validation" runs td-secret's:
//! `TD_TEST_SWTPM=/absolute/path/to/swtpm cargo test --manifest-path
//! td-tpm/Cargo.toml emulator_ -- --ignored`.

use super::*;
use std::cell::RefCell;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

const SHUTDOWN: u32 = 0x145;
/// TPM2_DictionaryAttackParameters and TPM_RH_LOCKOUT, sent here under
/// swtpm's empty lockoutAuth until the client carries the lockout
/// commands (td-install/ENCRYPTION.md increment 8a's next commit).
const DICTIONARY_ATTACK_PARAMETERS: u32 = 0x13a;
const LOCKOUT: u32 = 0x4000_000a;

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
    while !socket.exists() {
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

    /// td's DA parameters: 32 tries, recovery 600 s, lockoutRecovery a day.
    fn set_dictionary_attack(&self) {
        let mut parameters = Vec::new();
        for value in [32, 600, 86400] {
            put32(&mut parameters, value);
        }
        self.client()
            .call(
                DICTIONARY_ATTACK_PARAMETERS,
                &[LOCKOUT],
                Some(PASSWORD),
                &parameters,
                false,
            )
            .unwrap();
    }
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
fn emulator_locks_out_after_32_wrong_pins() {
    let tpm = Emulator::start("lockout");
    tpm.set_dictionary_attack();
    let state = tpm.client().dictionary_attack().unwrap();
    assert_eq!(
        (state.max_tries, state.interval, state.recovery),
        (32, 600, 86400)
    );
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
