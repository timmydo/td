//! Test-only CTAP 2.1 authenticator for td's subset, behind the transaction
//! Channel in process or, in a guest, `fido_uhid`'s HID device. It decrypts
//! and verifies every PIN-protocol message itself. No attestation chain or
//! real presence; a persistent key keeps its `State` in a file, saved before
//! each reply that changed it, so a guest's key survives a cold boot.

use crate::fido_cbor::{self as cbor, Encoder, Value};
use crate::fido_ctap::RP_ID;
use crate::fido_device::Interruption;
use crate::fido_hid::Message;
use crate::fido_p256::{fixture_scalar, PublicKey, SecretScalar};
use crate::fido_transaction::Channel;
use crate::{crypto, fido_aes};
use std::collections::VecDeque;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

const MAX_RETRIES: u8 = 8;
/// Consecutive mismatches before the key needs a power cycle.
const MAX_FAILURES: u8 = 3;

const INVALID_COMMAND: u8 = 0x01;
const INVALID_PARAMETER: u8 = 0x02;
const INVALID_CBOR: u8 = 0x12;
const MISSING_PARAMETER: u8 = 0x14;
const LIMIT_EXCEEDED: u8 = 0x15;
const CREDENTIAL_EXCLUDED: u8 = 0x19;
const UNSUPPORTED_ALGORITHM: u8 = 0x26;
const OPERATION_DENIED: u8 = 0x27;
const UNSUPPORTED_OPTION: u8 = 0x2b;
const INVALID_OPTION: u8 = 0x2c;
const NO_CREDENTIALS: u8 = 0x2e;
const USER_ACTION_TIMEOUT: u8 = 0x2f;
const PIN_INVALID: u8 = 0x31;
const PIN_BLOCKED: u8 = 0x32;
const PIN_AUTH_INVALID: u8 = 0x33;
const PIN_AUTH_BLOCKED: u8 = 0x34;
const PIN_NOT_SET: u8 = 0x35;
const PUAT_REQUIRED: u8 = 0x36;
const REQUEST_TOO_LARGE: u8 = 0x39;
const INVALID_SUBCOMMAND: u8 = 0x3e;
const UNAUTHORIZED_PERMISSION: u8 = 0x40;
/// CTAP2_ERR_OTHER: a persistent key that could not save its state.
const OTHER: u8 = 0x7f;

// pinUvAuthToken permissions.
const MAKE_CREDENTIAL: u64 = 1;
const GET_ASSERTION: u64 = 2;
const LARGE_BLOB_WRITE: u64 = 0x10;
/// mc, ga, cm, be, lbw and acfg; higher bits are ignored.
const DEFINED_PERMISSIONS: u64 = 0x3f;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Attestation {
    None,
    /// Self attestation by the new credential's own key.
    Packed,
}

/// The getInfo claims and fixed behaviour of one virtual key.
#[derive(Clone)]
pub(crate) struct Config {
    pub(crate) versions: &'static [&'static str],
    /// None omits getInfo's extensions member.
    pub(crate) extensions: Option<&'static [&'static str]>,
    /// False omits the clientPin option: the key cannot hold a PIN.
    pub(crate) pin_support: bool,
    pub(crate) always_uv: Option<bool>,
    /// The pinUvAuthToken option, which enables subcommand 9.
    pub(crate) permissions: bool,
    pub(crate) protocols: &'static [u64],
    pub(crate) max_message: Option<u64>,
    pub(crate) max_list: Option<u64>,
    pub(crate) max_id: Option<u64>,
    pub(crate) id_length: usize,
    pub(crate) token_length: usize,
    /// credProtect level of every new credential; 3 hides it without UV.
    pub(crate) cred_protect: Option<u8>,
    pub(crate) attestation: Attestation,
    pub(crate) aaguid: [u8; 16],
}

impl Default for Config {
    fn default() -> Self {
        Self {
            versions: &["FIDO_2_0", "FIDO_2_1"],
            extensions: Some(&["hmac-secret"]),
            pin_support: true,
            always_uv: None,
            permissions: true,
            protocols: &[1, 2],
            max_message: Some(1200),
            max_list: Some(8),
            max_id: Some(128),
            id_length: 64,
            token_length: 32,
            cred_protect: None,
            attestation: Attestation::None,
            aaguid: [0x5a; 16],
        }
    }
}

/// What survives a power cycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct State {
    pub(crate) pin: Option<Vec<u8>>,
    pub(crate) retries: u8,
    pub(crate) counter: u32,
    pub(crate) credentials: Vec<Credential>,
    /// Randomness is SHA-256 over the seed and a draw count.
    pub(crate) seed: Vec<u8>,
    pub(crate) draws: u64,
}

/// One non-resident ES256 credential for `RP_ID`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Credential {
    pub(crate) id: Vec<u8>,
    pub(crate) private: [u8; 32],
    /// hmac-secret's CredRandomWithUV and CredRandomWithoutUV.
    pub(crate) hmac: Option<([u8; 32], [u8; 32])>,
    pub(crate) protect: u8,
}

#[derive(Clone, Copy, Default)]
pub(crate) enum Presence {
    #[default]
    Granted,
    Delayed(Duration),
    /// Pending until `Virtual::release_touch`, or refused as a timeout
    /// after a minute.
    Held,
    Denied,
    Timeout,
}

#[derive(Clone, Copy, Default)]
pub(crate) enum Output {
    #[default]
    Derived,
    /// One bit flipped after derivation.
    Wrong,
    Fixed([u8; 32]),
}

#[derive(Clone, Copy, Default)]
pub(crate) enum Signing {
    #[default]
    Valid,
    /// The previous assertion's signature, replayed: a signature over other
    /// data, whatever the assertion it came from.
    Stale,
    /// The current authenticator data signed over the client-data hash of
    /// the last earlier assertion that asked for presence: valid exactly
    /// when the client sent that hash again, so a challenge reused across
    /// operations passes and a fresh one fails. With no earlier hash it
    /// signs the current one.
    Replayed,
    /// A key that is not the credential's.
    Foreign,
}

/// Scripted misbehaviour, changeable between operations.
#[derive(Clone, Copy, Default)]
pub(crate) struct Script {
    pub(crate) presence: Presence,
    pub(crate) output: Output,
    pub(crate) signing: Signing,
    /// BE and BS bits added to every authenticator data's flags.
    pub(crate) backup: u8,
}

struct Token {
    bytes: Vec<u8>,
    protocol: u64,
    permissions: u64,
    rp: Option<String>,
}

struct Authenticator {
    config: Config,
    state: State,
    script: Script,
    // Volatile: lost on power cycle.
    agreement: Option<SecretScalar>,
    token: Option<Token>,
    failures: u8,
    last_signature: Option<Vec<u8>>,
    /// The client-data hash of the last assertion that asked for presence,
    /// as one recorded off the wire: a power cycle keeps it.
    presence_hash: Option<Vec<u8>>,
    injected: VecDeque<Vec<u8>>,
    transcript: Vec<(Vec<u8>, Vec<u8>)>,
    /// The scripted presence delays, read without the lock.
    touch: Arc<Touch>,
    /// A persistent key's state file, the first save that failed, whether
    /// one failed after its rename, a fault a test set for the next save,
    /// and the alarm that refuses every save once the guest is being cut.
    file: Option<PathBuf>,
    unsaved: Option<String>,
    poisoned: bool,
    fault: Option<Fault>,
    frozen: Option<fn(&str)>,
}

/// Whether a scripted presence delay runs now, how long all of them have
/// taken, in microseconds, and whether a held one was released.
#[derive(Default)]
struct Touch {
    now: AtomicBool,
    total: AtomicU64,
    released: AtomicBool,
}

/// One virtual key; `link` is the channel a Transaction owns and drops.
/// A clone is the same key, which a HID binding serves from its own thread.
#[derive(Clone)]
pub(crate) struct Virtual {
    inner: Arc<Mutex<Authenticator>>,
    touch: Arc<Touch>,
}

impl Virtual {
    pub(crate) fn new(config: Config, pin: Option<&[u8]>, seed: &str) -> Self {
        Self::with(
            config,
            State {
                pin: pin.map(<[u8]>::to_vec),
                retries: MAX_RETRIES,
                counter: 0,
                credentials: Vec::new(),
                seed: seed.as_bytes().to_vec(),
                draws: 0,
            },
            None,
        )
    }

    /// A new key whose state lives in `file`, which must not exist yet: the
    /// state is saved now and after every request or edit that changes it,
    /// before the reply, as a real key's flash write precedes its reply.
    pub(crate) fn persistent(
        config: Config,
        pin: Option<&[u8]>,
        seed: &str,
        file: &Path,
    ) -> Result<Self, String> {
        let key = Self::new(config, pin, seed);
        if fs::symlink_metadata(file).is_ok() {
            return Err(format!("virtual key state {} exists", file.display()));
        }
        let state = key.state();
        save(file, &state, None).map_err(|error| match error {
            Unsaved::Before(error) | Unsaved::After(error) => error,
        })?;
        key.lock().file = Some(file.to_path_buf());
        Ok(key)
    }

    /// The persistent key saved in `file`, as a cold boot finds it: its
    /// volatile PIN-token, agreement and consecutive-failure state is gone.
    pub(crate) fn restore(config: Config, file: &Path) -> Result<Self, Damage> {
        let state = load(file)?;
        Ok(Self::with(config, state, Some(file.to_path_buf())))
    }

    fn with(config: Config, state: State, file: Option<PathBuf>) -> Self {
        let touch = Arc::new(Touch::default());
        let inner = Arc::new(Mutex::new(Authenticator {
            config,
            state,
            script: Script::default(),
            agreement: None,
            token: None,
            failures: 0,
            last_signature: None,
            presence_hash: None,
            injected: VecDeque::new(),
            transcript: Vec::new(),
            touch: Arc::clone(&touch),
            file,
            unsaved: None,
            poisoned: false,
            fault: None,
            frozen: None,
        }));
        Self { inner, touch }
    }

    /// The save that poisoned a persistent key, else its first failed save;
    /// either answered its request with CTAP2_ERR_OTHER
    /// (`Authenticator::persist`).
    pub(crate) fn saved(&self) -> Result<(), String> {
        let inner = self.lock();
        match (&inner.unsaved, inner.poisoned) {
            (None, _) => Ok(()),
            (Some(error), false) => Err(error.clone()),
            (Some(error), true) => Err(format!(
                "{error}; poisoned after its rename, the key refuses every request"
            )),
        }
    }

    /// Makes the next save fail at `fault`.
    pub(crate) fn fail_next_save(&self, fault: Fault) {
        self.lock().fault = Some(fault);
    }

    /// From now on every save is refused before it writes anything, and
    /// `alarm` is told: a guest cut must see no key sync after its write
    /// began.
    pub(crate) fn freeze(&self, alarm: fn(&str)) {
        self.lock().frozen = Some(alarm);
    }

    fn lock(&self) -> MutexGuard<'_, Authenticator> {
        self.inner.lock().unwrap()
    }

    pub(crate) fn link(&self) -> Link {
        Link(Arc::clone(&self.inner))
    }

    /// Simulated reinsertion: clears PIN AUTH BLOCKED, never PIN BLOCKED.
    pub(crate) fn power_cycle(&self) {
        let mut inner = self.lock();
        inner.agreement = None;
        inner.token = None;
        inner.failures = 0;
        inner.last_signature = None;
    }

    pub(crate) fn state(&self) -> State {
        self.lock().state.clone()
    }

    /// A test's edit, saved as a request's change is; a poisoned key takes
    /// none.
    pub(crate) fn with_state(&self, change: impl FnOnce(&mut State)) {
        let mut inner = self.lock();
        if inner.poisoned {
            return;
        }
        let before = inner.file.is_some().then(|| inner.state.clone());
        change(&mut inner.state);
        if let Some(before) = before {
            inner.persist(before);
        }
    }

    pub(crate) fn script(&self, script: Script) {
        self.lock().script = script;
    }

    /// Changes the getInfo claims and fixed behaviour between operations, as
    /// a real key's configuration tool can, such as toggling alwaysUv.
    pub(crate) fn configure(&self, change: impl FnOnce(&mut Config)) {
        change(&mut self.lock().config);
    }

    /// Replaces the next random draw, which must have this length.
    pub(crate) fn inject(&self, bytes: Vec<u8>) {
        self.lock().injected.push_back(bytes);
    }

    pub(crate) fn transcript(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.lock().transcript.clone()
    }

    pub(crate) fn exchange(&self, request: &[u8]) -> Vec<u8> {
        self.lock().handle(request)
    }

    /// Whether a request waits on its scripted presence delay, which a HID
    /// binding reports in its keepalives as UPNEEDED.
    pub(crate) fn touching(&self) -> bool {
        self.touch.now.load(Ordering::Acquire)
    }

    /// How long the key's scripted presence delays have actually taken.
    pub(crate) fn touched(&self) -> Duration {
        Duration::from_micros(self.touch.total.load(Ordering::Acquire))
    }

    /// Completes a `Presence::Held` request: the touch.
    pub(crate) fn release_touch(&self) {
        self.touch.released.store(true, Ordering::Release);
    }
}

pub(crate) struct Link(Arc<Mutex<Authenticator>>);

impl Channel for Link {
    fn check(&self) -> Result<(), Interruption> {
        Ok(())
    }
    fn exchange(&mut self, request: &[u8]) -> Result<Message, String> {
        let reply = self.0.lock().unwrap().handle(request);
        Ok(crate::fido_transaction::tests::message(&reply))
    }
}

/// One PIN protocol's keys from an ECDH x coordinate (CTAP 2.1 section 6.5.6/7).
struct Shared {
    protocol: u64,
    aes: [u8; 32],
    hmac: [u8; 32],
}

impl Shared {
    fn derive(protocol: u64, z: &[u8; 32]) -> Self {
        if protocol == 1 {
            let key = crypto::digest(z);
            return Self {
                protocol,
                aes: key,
                hmac: key,
            };
        }
        Self {
            protocol,
            aes: crypto::hkdf(z, &[0; 32], b"CTAP2 AES key"),
            hmac: crypto::hkdf(z, &[0; 32], b"CTAP2 HMAC key"),
        }
    }

    /// Protocol 1 ignores the IV and uses zero; protocol 2 prefixes it.
    fn seal(&self, iv: &[u8; 16], plain: &[u8]) -> Vec<u8> {
        let mut body = plain.to_vec();
        if self.protocol == 1 {
            fido_aes::encrypt(&self.aes, &[0; 16], &mut body).unwrap();
            return body;
        }
        fido_aes::encrypt(&self.aes, iv, &mut body).unwrap();
        [iv.as_slice(), &body].concat()
    }

    fn open(&self, sealed: &[u8]) -> Option<Vec<u8>> {
        let (iv, body) = if self.protocol == 1 {
            ([0; 16], sealed)
        } else {
            let (iv, body) = sealed.split_at_checked(16)?;
            (iv.try_into().ok()?, body)
        };
        let mut out = body.to_vec();
        fido_aes::decrypt(&self.aes, &iv, &mut out).ok()?;
        Some(out)
    }

    fn tag(&self, message: &[u8]) -> Vec<u8> {
        authenticate(self.protocol, &self.hmac, message)
    }
}

fn authenticate(protocol: u64, key: &[u8], message: &[u8]) -> Vec<u8> {
    let mac = crypto::hmac(key, message);
    mac[..if protocol == 1 { 16 } else { 32 }].to_vec()
}

fn build(write: impl FnOnce(&mut Encoder) -> Result<(), String>) -> Vec<u8> {
    let mut out = Encoder::new();
    write(&mut out).unwrap();
    out.finish().unwrap()
}

/// COSE EC2 P-256 with a negative algorithm argument: 24 is ECDH, 6 is ES256.
fn encode_key(out: &mut Encoder, algorithm: u64, key: &PublicKey) -> Result<(), String> {
    let (x, y) = key.coordinates();
    out.head(5, 5)?;
    out.head(0, 1)?;
    out.head(0, 2)?;
    out.head(0, 3)?;
    out.head(1, algorithm)?;
    out.head(1, 0)?;
    out.head(0, 1)?;
    out.head(1, 1)?;
    out.bytes(&x)?;
    out.head(1, 2)?;
    out.bytes(&y)
}

fn field<'v, 'a>(value: &'v Value<'a>, key: Value<'a>) -> Result<Option<&'v Value<'a>>, u8> {
    value.get(&key).map_err(|_| INVALID_CBOR)
}

fn unsigned(value: &Value<'_>, key: u64) -> Result<Option<u64>, u8> {
    field(value, Value::Unsigned(key))?
        .map(|value| value.unsigned().map_err(|_| INVALID_CBOR))
        .transpose()
}

fn bytes<'a>(value: &Value<'a>, key: u64) -> Result<Option<&'a [u8]>, u8> {
    field(value, Value::Unsigned(key))?
        .map(|value| value.bytes().map_err(|_| INVALID_CBOR))
        .transpose()
}

fn text<'a>(value: &Value<'a>, key: Value<'a>) -> Result<Option<&'a str>, u8> {
    field(value, key)?
        .map(|value| value.text().map_err(|_| INVALID_CBOR))
        .transpose()
}

/// rk (None when absent), up and uv, with their CTAP defaults.
fn options(value: &Value<'_>, key: u64) -> Result<(Option<bool>, bool, bool), u8> {
    let mut found = [None, Some(true), Some(false)];
    if let Some(options) = field(value, Value::Unsigned(key))? {
        for (name, slot) in ["rk", "up", "uv"].into_iter().zip(&mut found) {
            match field(options, Value::Text(name))? {
                None => {}
                Some(Value::Simple(20)) => *slot = Some(false),
                Some(Value::Simple(21)) => *slot = Some(true),
                Some(_) => return Err(INVALID_CBOR),
            }
        }
    }
    let [rk, up, uv] = found;
    Ok((rk, up == Some(true), uv == Some(true)))
}

/// Public-key descriptor IDs; an absent list is empty.
fn descriptors<'a>(value: &Value<'a>, key: u64) -> Result<Vec<&'a [u8]>, u8> {
    let Some(list) = field(value, Value::Unsigned(key))? else {
        return Ok(Vec::new());
    };
    let Value::Array(items) = list else {
        return Err(INVALID_CBOR);
    };
    let mut ids = Vec::with_capacity(items.len());
    for item in items {
        if text(item, Value::Text("type"))? != Some("public-key") {
            return Err(INVALID_PARAMETER);
        }
        let id = field(item, Value::Text("id"))?.ok_or(MISSING_PARAMETER)?;
        ids.push(id.bytes().map_err(|_| INVALID_CBOR)?);
    }
    Ok(ids)
}

fn agreement_key(cose: &Value<'_>) -> Option<PublicKey> {
    if field(cose, Value::Unsigned(1)).ok()? != Some(&Value::Unsigned(2))
        || field(cose, Value::Unsigned(3)).ok()? != Some(&Value::Negative(24))
        || field(cose, Value::Negative(0)).ok()? != Some(&Value::Unsigned(1))
    {
        return None;
    }
    let coordinate = |key| -> Option<[u8; 32]> {
        field(cose, Value::Negative(key))
            .ok()??
            .bytes()
            .ok()?
            .try_into()
            .ok()
    };
    PublicKey::from_coordinates(&coordinate(1)?, &coordinate(2)?).ok()
}

impl Authenticator {
    /// A poisoned key answers every request with CTAP2_ERR_OTHER.
    fn handle(&mut self, request: &[u8]) -> Vec<u8> {
        let reply = if self.poisoned {
            vec![OTHER]
        } else {
            let before = self.file.is_some().then(|| self.state.clone());
            let reply = match self.dispatch(request) {
                Ok(body) => [&[0][..], &body].concat(),
                Err(status) => vec![status],
            };
            let saved = match before {
                Some(before) => self.persist(before),
                None => true,
            };
            if saved {
                reply
            } else {
                vec![OTHER]
            }
        };
        self.transcript.push((request.to_vec(), reply.clone()));
        reply
    }

    /// Saves a persistent key's changed state, and says whether it did.
    /// A save refused or failed before the rename leaves the file as it
    /// was, so the state goes back to `before`, and the agreement key and
    /// PIN token, which may come from draws it rewinds, go too: no rewound
    /// draw stays in use, and none reached a reply. The consecutive-failure
    /// count and a consumed injected draw stay. A failure after the rename
    /// leaves the file holding the new state or the old, unknown which, so
    /// the key is poisoned: it keeps the new state in memory and refuses
    /// every request from then on.
    fn persist(&mut self, before: State) -> bool {
        let Some(file) = self.file.clone() else {
            return true;
        };
        if self.state == before {
            return true;
        }
        let result = match self.frozen {
            Some(alarm) => {
                let refused = "virtual key save refused: the guest is being cut".to_string();
                alarm(&refused);
                Err(Unsaved::Before(refused))
            }
            None => save(&file, &self.state, self.fault.take()),
        };
        match result {
            Ok(()) => true,
            Err(Unsaved::Before(error)) => {
                self.state = before;
                self.agreement = None;
                self.token = None;
                self.unsaved.get_or_insert(error);
                false
            }
            Err(Unsaved::After(error)) => {
                self.poisoned = true;
                self.unsaved = Some(error);
                false
            }
        }
    }

    fn dispatch(&mut self, request: &[u8]) -> Result<Vec<u8>, u8> {
        if request.len() as u64 > self.config.max_message.unwrap_or(1024) {
            return Err(REQUEST_TOO_LARGE);
        }
        let decode = |body| cbor::decode(body).map_err(|_| INVALID_CBOR);
        match request.split_first() {
            Some((4, [])) => Ok(self.info()),
            Some((1, body)) => self.make_credential(&decode(body)?),
            Some((2, body)) => self.get_assertion(&decode(body)?),
            Some((6, body)) => self.client_pin(&decode(body)?),
            _ => Err(INVALID_COMMAND),
        }
    }

    fn draw(&mut self, length: usize) -> Vec<u8> {
        if let Some(bytes) = self.injected.pop_front() {
            assert_eq!(bytes.len(), length, "injected draw length");
            return bytes;
        }
        let mut out = Vec::with_capacity(length + 32);
        while out.len() < length {
            self.state.draws += 1;
            let mut input = self.state.seed.clone();
            input.extend(self.state.draws.to_be_bytes());
            out.extend(crypto::digest(&input));
        }
        out.truncate(length);
        out
    }

    /// A drawn scalar out of range is reduced; an injected one must be valid.
    fn draw_private(&mut self) -> [u8; 32] {
        let injected = !self.injected.is_empty();
        let bytes: [u8; 32] = self.draw(32).try_into().unwrap();
        if SecretScalar::from_bytes(Box::new(bytes)).is_ok() {
            return bytes;
        }
        assert!(!injected, "injected P-256 scalar out of range");
        fixture_scalar(&bytes)
    }

    fn agreement(&mut self) -> &SecretScalar {
        if self.agreement.is_none() {
            let private = self.draw_private();
            self.agreement = Some(SecretScalar::from_bytes(Box::new(private)).unwrap());
        }
        self.agreement.as_ref().unwrap()
    }

    fn shared(&mut self, protocol: u64, cose: &Value<'_>) -> Result<Shared, u8> {
        if !self.config.protocols.contains(&protocol) {
            return Err(INVALID_PARAMETER);
        }
        let platform = agreement_key(cose).ok_or(INVALID_PARAMETER)?;
        let z = self
            .agreement()
            .agree(&platform)
            .map_err(|_| INVALID_PARAMETER)?;
        Ok(Shared::derive(protocol, z.bytes()))
    }

    fn seal(&mut self, shared: &Shared, plain: &[u8]) -> Vec<u8> {
        let iv = if shared.protocol == 1 {
            [0; 16]
        } else {
            self.draw(16).try_into().unwrap()
        };
        shared.seal(&iv, plain)
    }

    fn presence(&self) -> Result<(), u8> {
        match self.script.presence {
            Presence::Granted => Ok(()),
            Presence::Delayed(delay) => {
                let started = std::time::Instant::now();
                self.touch.now.store(true, Ordering::Release);
                std::thread::sleep(delay);
                self.touch.now.store(false, Ordering::Release);
                let took = u64::try_from(started.elapsed().as_micros()).unwrap();
                self.touch.total.fetch_add(took, Ordering::AcqRel);
                Ok(())
            }
            Presence::Held => {
                let started = std::time::Instant::now();
                self.touch.now.store(true, Ordering::Release);
                while !self.touch.released.swap(false, Ordering::AcqRel) {
                    if started.elapsed() >= Duration::from_secs(60) {
                        self.touch.now.store(false, Ordering::Release);
                        return Err(USER_ACTION_TIMEOUT);
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                self.touch.now.store(false, Ordering::Release);
                let took = u64::try_from(started.elapsed().as_micros()).unwrap();
                self.touch.total.fetch_add(took, Ordering::AcqRel);
                Ok(())
            }
            Presence::Denied => Err(OPERATION_DENIED),
            Presence::Timeout => Err(USER_ACTION_TIMEOUT),
        }
    }

    /// makeCredential's and getAssertion's presence step. On a CTAP 2.1 key a
    /// token in use then keeps only lbw (sections 6.1.2 step 14, 6.2.2 step 9).
    fn collect_presence(&mut self) -> Result<(), u8> {
        self.presence()?;
        if self.single_use() {
            if let Some(token) = self.token.as_mut() {
                token.permissions &= LARGE_BLOB_WRITE;
            }
        }
        Ok(())
    }

    fn info(&self) -> Vec<u8> {
        let config = &self.config;
        let mut options = vec![("rk", false), ("up", true), ("plat", false)];
        if config.pin_support {
            options.push(("clientPin", self.state.pin.is_some()));
        }
        options.push(("pinUvAuthToken", config.permissions));
        options.extend(config.always_uv.map(|value| ("alwaysUv", value)));
        // Canonical CBOR: shorter text keys first, then bytewise.
        options.sort_by_key(|(name, _)| (name.len(), *name));
        let optional = [
            config.extensions.is_some(),
            config.max_message.is_some(),
            config.max_list.is_some(),
            config.max_id.is_some(),
        ];
        build(|out| {
            out.head(
                5,
                4 + optional.iter().filter(|present| **present).count() as u64,
            )?;
            out.head(0, 1)?;
            out.head(4, config.versions.len() as u64)?;
            for version in config.versions {
                out.text(version)?;
            }
            if let Some(extensions) = config.extensions {
                out.head(0, 2)?;
                out.head(4, extensions.len() as u64)?;
                for extension in extensions {
                    out.text(extension)?;
                }
            }
            out.head(0, 3)?;
            out.bytes(&config.aaguid)?;
            out.head(0, 4)?;
            out.head(5, options.len() as u64)?;
            for (name, value) in &options {
                out.text(name)?;
                out.boolean(*value)?;
            }
            if let Some(size) = config.max_message {
                out.head(0, 5)?;
                out.head(0, size)?;
            }
            out.head(0, 6)?;
            out.head(4, config.protocols.len() as u64)?;
            for protocol in config.protocols {
                out.head(0, *protocol)?;
            }
            for (key, limit) in [(7, config.max_list), (8, config.max_id)] {
                if let Some(limit) = limit {
                    out.head(0, key)?;
                    out.head(0, limit)?;
                }
            }
            Ok(())
        })
    }

    fn client_pin(&mut self, value: &Value<'_>) -> Result<Vec<u8>, u8> {
        let protocol = unsigned(value, 1)?;
        if protocol.is_some_and(|protocol| !self.config.protocols.contains(&protocol)) {
            return Err(INVALID_PARAMETER);
        }
        match unsigned(value, 2)?.ok_or(MISSING_PARAMETER)? {
            1 => {
                let (retries, blocked) = (self.state.retries, self.failures >= MAX_FAILURES);
                Ok(build(|out| {
                    out.head(5, 2)?;
                    out.head(0, 3)?;
                    out.head(0, u64::from(retries))?;
                    out.head(0, 4)?;
                    out.boolean(blocked)
                }))
            }
            2 => {
                protocol.ok_or(MISSING_PARAMETER)?;
                let public = self.agreement().public_key().unwrap();
                Ok(build(|out| {
                    out.head(5, 1)?;
                    out.head(0, 1)?;
                    encode_key(out, 24, &public)
                }))
            }
            5 => self.pin_token(value, protocol.ok_or(MISSING_PARAMETER)?, false),
            9 if self.config.permissions => {
                self.pin_token(value, protocol.ok_or(MISSING_PARAMETER)?, true)
            }
            _ => Err(INVALID_SUBCOMMAND),
        }
    }

    /// getPinToken (legacy) or getPinUvAuthTokenUsingPinWithPermissions.
    fn pin_token(&mut self, value: &Value<'_>, protocol: u64, scoped: bool) -> Result<Vec<u8>, u8> {
        // client_pin refused an unsupported protocol first; here missing
        // parameters precede invalid ones.
        let platform = field(value, Value::Unsigned(3))?.ok_or(MISSING_PARAMETER)?;
        let pin_hash = bytes(value, 6)?.ok_or(MISSING_PARAMETER)?;
        let (permissions, rp) = if scoped {
            let requested = unsigned(value, 9)?.ok_or(MISSING_PARAMETER)?;
            let rp = text(value, Value::Unsigned(10))?;
            // The permission table marks rpId Required for mc and ga.
            if requested & (MAKE_CREDENTIAL | GET_ASSERTION) != 0 && rp.is_none() {
                return Err(MISSING_PARAMETER);
            }
            if requested == 0 {
                return Err(INVALID_PARAMETER);
            }
            let permissions = requested & DEFINED_PERMISSIONS;
            if permissions & !(MAKE_CREDENTIAL | GET_ASSERTION) != 0 {
                return Err(UNAUTHORIZED_PERMISSION);
            }
            (permissions, rp.map(str::to_string))
        } else {
            if field(value, Value::Unsigned(9))?.is_some()
                || field(value, Value::Unsigned(10))?.is_some()
            {
                return Err(INVALID_PARAMETER);
            }
            (MAKE_CREDENTIAL | GET_ASSERTION, None)
        };
        let pin = self.state.pin.clone().ok_or(PIN_NOT_SET)?;
        if self.state.retries == 0 {
            return Err(PIN_BLOCKED);
        }
        if self.failures >= MAX_FAILURES {
            return Err(PIN_AUTH_BLOCKED);
        }
        let shared = self.shared(protocol, platform)?;
        self.state.retries -= 1;
        let expected = &crypto::digest(&pin)[..16];
        if shared.open(pin_hash).as_deref() != Some(expected) {
            self.agreement = None;
            self.failures += 1;
            if self.state.retries == 0 {
                return Err(PIN_BLOCKED);
            }
            if self.failures >= MAX_FAILURES {
                return Err(PIN_AUTH_BLOCKED);
            }
            return Err(PIN_INVALID);
        }
        self.state.retries = MAX_RETRIES;
        self.failures = 0;
        let token = self.draw(self.config.token_length);
        let encrypted = self.seal(&shared, &token);
        self.token = Some(Token {
            bytes: token,
            protocol,
            permissions,
            rp,
        });
        Ok(build(|out| {
            out.head(5, 1)?;
            out.head(0, 2)?;
            out.bytes(&encrypted)
        }))
    }

    /// A CTAP 2.1 token loses its permissions once a request collects
    /// presence; a 2.0 pinToken stays valid until the next getPinToken or
    /// power cycle.
    fn single_use(&self) -> bool {
        self.config.versions.contains(&"FIDO_2_1")
    }

    /// Whether a present pinUvAuthParam verified, which grants UV. Every
    /// refusal is PIN_AUTH_INVALID, as CTAP 2.1 sections 6.1.2 and 6.2.2 say.
    fn authorized(
        &mut self,
        value: &Value<'_>,
        rp: &str,
        hash: &[u8],
        keys: (u64, u64),
        permission: u64,
    ) -> Result<bool, u8> {
        let Some(param) = bytes(value, keys.0)? else {
            return Ok(false);
        };
        let protocol = unsigned(value, keys.1)?.ok_or(MISSING_PARAMETER)?;
        if !self.config.protocols.contains(&protocol) {
            return Err(INVALID_PARAMETER);
        }
        let single_use = self.single_use();
        let token = self.token.as_mut().ok_or(PIN_AUTH_INVALID)?;
        if token.protocol != protocol || authenticate(protocol, &token.bytes, hash) != param {
            return Err(PIN_AUTH_INVALID);
        }
        if token.permissions & permission == 0
            || token.rp.as_deref().is_some_and(|bound| bound != rp)
        {
            return Err(PIN_AUTH_INVALID);
        }
        // A legacy getPinToken token takes the RP of its first use.
        if single_use && token.rp.is_none() {
            token.rp = Some(rp.to_string());
        }
        Ok(true)
    }

    fn check_list(&self, ids: &[&[u8]]) -> Result<(), u8> {
        match self.config.max_list {
            Some(limit) if ids.len() as u64 > limit => Err(LIMIT_EXCEEDED),
            _ => Ok(()),
        }
    }

    /// Level 3 credProtect hides a credential from any request without UV.
    fn visible(&self, id: &[u8], uv: bool) -> Option<usize> {
        let index = self.state.credentials.iter().position(|c| c.id == id)?;
        (self.state.credentials[index].protect < 3 || uv).then_some(index)
    }

    fn make_credential(&mut self, value: &Value<'_>) -> Result<Vec<u8>, u8> {
        let hash = bytes(value, 1)?.ok_or(MISSING_PARAMETER)?;
        let rp = field(value, Value::Unsigned(2))?.ok_or(MISSING_PARAMETER)?;
        if text(rp, Value::Text("id"))? != Some(RP_ID) {
            return Err(INVALID_PARAMETER);
        }
        field(value, Value::Unsigned(3))?.ok_or(MISSING_PARAMETER)?;
        let Some(Value::Array(algorithms)) = field(value, Value::Unsigned(4))? else {
            return Err(MISSING_PARAMETER);
        };
        let es256 = algorithms.iter().any(|entry| {
            field(entry, Value::Text("alg")) == Ok(Some(&Value::Negative(6)))
                && text(entry, Value::Text("type")) == Ok(Some("public-key"))
        });
        if !es256 {
            return Err(UNSUPPORTED_ALGORITHM);
        }
        let (rk, up, uv_option) = options(value, 7)?;
        // uv is ignored beside pinUvAuthParam; there is no built-in UV.
        if uv_option && bytes(value, 8)?.is_none() {
            return Err(INVALID_OPTION);
        }
        if rk == Some(true) {
            return Err(UNSUPPORTED_OPTION);
        }
        if !up {
            return Err(INVALID_OPTION);
        }
        let uv = self.authorized(value, RP_ID, hash, (8, 9), MAKE_CREDENTIAL)?;
        if !uv && self.state.pin.is_some() {
            return Err(PUAT_REQUIRED);
        }
        let excluded = descriptors(value, 5)?;
        self.check_list(&excluded)?;
        if excluded.iter().any(|id| self.visible(id, uv).is_some()) {
            // Excluded whatever the presence outcome; the token is untouched.
            let _ = self.presence();
            return Err(CREDENTIAL_EXCLUDED);
        }
        self.collect_presence()?;
        let hmac_secret = match field(value, Value::Unsigned(6))? {
            Some(extensions) => {
                field(extensions, Value::Text("hmac-secret"))? == Some(&Value::Simple(21))
            }
            None => false,
        };
        let id = self.draw(self.config.id_length);
        let private = self.draw_private();
        let hmac = hmac_secret.then(|| {
            let with_uv = self.draw(32).try_into().unwrap();
            (with_uv, self.draw(32).try_into().unwrap())
        });
        let key = SecretScalar::from_bytes(Box::new(private)).unwrap();
        let public = key.public_key().unwrap();
        let flags = 0x41 | if uv { 0x04 } else { 0 } | if hmac_secret { 0x80 } else { 0 };
        let mut data = crypto::digest(RP_ID.as_bytes()).to_vec();
        data.push(flags | self.script.backup);
        data.extend(self.state.counter.to_be_bytes());
        data.extend(self.config.aaguid);
        data.extend((id.len() as u16).to_be_bytes());
        data.extend(&id);
        data.extend(build(|out| encode_key(out, 6, &public)));
        if hmac_secret {
            data.extend(build(|out| {
                out.head(5, 1)?;
                out.text("hmac-secret")?;
                out.boolean(true)
            }));
        }
        let statement = match self.config.attestation {
            Attestation::None => None,
            Attestation::Packed => Some(sign(&key, &data, hash)),
        };
        self.state.credentials.push(Credential {
            id,
            private,
            hmac,
            protect: self.config.cred_protect.unwrap_or(1),
        });
        Ok(build(|out| {
            out.head(5, 3)?;
            out.head(0, 1)?;
            out.text(if statement.is_some() {
                "packed"
            } else {
                "none"
            })?;
            out.head(0, 2)?;
            out.bytes(&data)?;
            out.head(0, 3)?;
            let Some(signature) = &statement else {
                return out.head(5, 0);
            };
            out.head(5, 2)?;
            out.text("alg")?;
            out.head(1, 6)?;
            out.text("sig")?;
            out.bytes(signature)
        }))
    }

    fn get_assertion(&mut self, value: &Value<'_>) -> Result<Vec<u8>, u8> {
        let rp = text(value, Value::Unsigned(1))?.ok_or(MISSING_PARAMETER)?;
        let hash = bytes(value, 2)?.ok_or(MISSING_PARAMETER)?;
        let (rk, up, uv_option) = options(value, 5)?;
        if uv_option && bytes(value, 6)?.is_none() {
            return Err(INVALID_OPTION);
        }
        if rk.is_some() {
            return Err(UNSUPPORTED_OPTION);
        }
        let uv = self.authorized(value, rp, hash, (6, 7), GET_ASSERTION)?;
        if up && !uv && self.config.always_uv == Some(true) {
            return Err(PUAT_REQUIRED);
        }
        let allowed = descriptors(value, 3)?;
        self.check_list(&allowed)?;
        let index = allowed
            .iter()
            .filter(|_| rp == RP_ID)
            .find_map(|id| self.visible(id, uv))
            .ok_or(NO_CREDENTIALS)?;
        if up {
            self.collect_presence()?;
        }
        let extension = match field(value, Value::Unsigned(4))? {
            Some(extensions) => field(extensions, Value::Text("hmac-secret"))?,
            None => None,
        };
        let secret = match extension {
            // Section 12.5: hmac-secret requires user presence.
            Some(_) if !up => return Err(UNSUPPORTED_OPTION),
            Some(input) => self.hmac_secret(input, index, uv)?,
            None => None,
        };
        let mut flags = u8::from(up) | if uv { 0x04 } else { 0 } | self.script.backup;
        if secret.is_some() {
            flags |= 0x80;
        }
        self.state.counter += 1;
        let mut data = crypto::digest(RP_ID.as_bytes()).to_vec();
        data.push(flags);
        data.extend(self.state.counter.to_be_bytes());
        if let Some(secret) = &secret {
            data.extend(build(|out| {
                out.head(5, 1)?;
                out.text("hmac-secret")?;
                out.bytes(secret)
            }));
        }
        let signature = self.signature(index, &data, hash, up);
        let id = self.state.credentials[index].id.clone();
        Ok(build(|out| {
            out.head(5, 3)?;
            out.head(0, 1)?;
            out.head(5, 2)?;
            out.text("id")?;
            out.bytes(&id)?;
            out.text("type")?;
            out.text("public-key")?;
            out.head(0, 2)?;
            out.bytes(&data)?;
            out.head(0, 3)?;
            out.bytes(&signature)
        }))
    }

    /// Verifies saltAuth and decrypts the salts before deriving any output.
    fn hmac_secret(
        &mut self,
        input: &Value<'_>,
        index: usize,
        uv: bool,
    ) -> Result<Option<Vec<u8>>, u8> {
        let platform = field(input, Value::Unsigned(1))?.ok_or(MISSING_PARAMETER)?;
        let salt_enc = bytes(input, 2)?.ok_or(MISSING_PARAMETER)?;
        let salt_auth = bytes(input, 3)?.ok_or(MISSING_PARAMETER)?;
        let shared = self.shared(unsigned(input, 4)?.unwrap_or(1), platform)?;
        if shared.tag(salt_enc) != salt_auth {
            return Err(PIN_AUTH_INVALID);
        }
        let salts = shared.open(salt_enc).ok_or(INVALID_PARAMETER)?;
        if !matches!(salts.len(), 32 | 64) {
            return Err(INVALID_PARAMETER);
        }
        let Some((with_uv, without_uv)) = self.state.credentials[index].hmac else {
            return Ok(None);
        };
        let random = if uv { with_uv } else { without_uv };
        let mut output: Vec<u8> = salts
            .chunks(32)
            .flat_map(|salt| crypto::hmac(&random, salt))
            .collect();
        match self.script.output {
            Output::Derived => {}
            Output::Wrong => output[0] ^= 1,
            Output::Fixed(fixed) => output[..32].copy_from_slice(&fixed),
        }
        Ok(Some(self.seal(&shared, &output)))
    }

    fn signature(&mut self, index: usize, data: &[u8], hash: &[u8], up: bool) -> Vec<u8> {
        let private = match self.script.signing {
            Signing::Foreign => fixture_scalar(&crypto::digest(b"foreign signer")),
            Signing::Valid | Signing::Stale | Signing::Replayed => {
                self.state.credentials[index].private
            }
        };
        let signed = match (self.script.signing, &self.presence_hash) {
            (Signing::Replayed, Some(earlier)) => earlier.clone(),
            _ => hash.to_vec(),
        };
        if up {
            self.presence_hash = Some(hash.to_vec());
        }
        let fresh = sign(
            &SecretScalar::from_bytes(Box::new(private)).unwrap(),
            data,
            &signed,
        );
        let previous = self.last_signature.replace(fresh.clone());
        match (self.script.signing, previous) {
            (Signing::Stale, Some(previous)) => previous,
            _ => fresh,
        }
    }
}

/// ES256 over data and the client-data hash, with pin_vectors.py's nonce rule.
fn sign(key: &SecretScalar, data: &[u8], hash: &[u8]) -> Vec<u8> {
    let digest = crypto::digest(&[data, hash].concat());
    let nonce = fixture_scalar(&crypto::digest(
        &[b"public-pin-signature".as_slice(), &digest].concat(),
    ));
    key.sign(&digest, &nonce).unwrap()
}

/// A persistent key's state file (td-secret/DESIGN.md, "Virtual
/// authenticator"): this magic, the body's u32 length, the body and the
/// SHA-256 of everything before it. Integers are big-endian.
const MAGIC: &[u8; 8] = b"TDVKEY01";
const MAX_FILE: usize = 64 * 1024;
const MAX_CREDENTIALS: usize = 32;
/// td-secret's CTAP credential bound, as the login record's.
const MAX_ID: usize = 1024;
const DIGEST: usize = 32;
const HEADER: usize = MAGIC.len() + 4;

/// Why a state file was refused: a damaged file never loads, and decoding
/// never panics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Damage {
    Unreadable(String),
    Oversized,
    Magic,
    /// Shorter than its header, its declared body or a field.
    Truncated,
    /// Longer than its declared body, or a body with bytes past its fields.
    Trailing,
    Digest,
    /// A field outside its bounds, by name.
    Field(&'static str),
}

impl fmt::Display for Damage {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable(error) => write!(out, "unreadable: {error}"),
            Self::Oversized => write!(out, "larger than {MAX_FILE} bytes"),
            Self::Magic => write!(out, "not a virtual key state"),
            Self::Truncated => write!(out, "truncated"),
            Self::Trailing => write!(out, "trailing bytes"),
            Self::Digest => write!(out, "digest mismatch"),
            Self::Field(name) => write!(out, "{name} out of bounds"),
        }
    }
}

/// The state's bytes; only a state that decodes back to itself encodes.
fn encode(state: &State) -> Result<Vec<u8>, Damage> {
    let short = |name, length: usize| u8::try_from(length).map_err(|_| Damage::Field(name));
    let mut body = Vec::new();
    match &state.pin {
        None => body.push(0),
        Some(pin) => {
            body.extend([1, short("pin", pin.len())?]);
            body.extend(pin);
        }
    }
    body.push(state.retries);
    body.extend(state.counter.to_be_bytes());
    body.extend(state.draws.to_be_bytes());
    body.push(short("seed", state.seed.len())?);
    body.extend(&state.seed);
    body.push(short("credentials", state.credentials.len())?);
    for credential in &state.credentials {
        let length =
            u16::try_from(credential.id.len()).map_err(|_| Damage::Field("credential ID"))?;
        body.extend(length.to_be_bytes());
        body.extend(&credential.id);
        body.extend(credential.private);
        match &credential.hmac {
            None => body.push(0),
            Some((with_uv, without_uv)) => {
                body.push(1);
                body.extend(with_uv);
                body.extend(without_uv);
            }
        }
        body.push(credential.protect);
    }
    let length = u32::try_from(body.len()).map_err(|_| Damage::Oversized)?;
    let mut bytes = MAGIC.to_vec();
    bytes.extend(length.to_be_bytes());
    bytes.extend(body);
    let digest = crypto::digest(&bytes);
    bytes.extend(digest);
    if decode(&bytes)? != *state {
        return Err(Damage::Field("state"));
    }
    Ok(bytes)
}

/// Fields read in order; running short is truncation.
struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], Damage> {
        let (head, rest) = self.0.split_at_checked(length).ok_or(Damage::Truncated)?;
        self.0 = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Damage> {
        self.take(N)?.try_into().map_err(|_| Damage::Truncated)
    }

    fn byte(&mut self) -> Result<u8, Damage> {
        let [byte] = self.array()?;
        Ok(byte)
    }

    fn flag(&mut self, name: &'static str) -> Result<bool, Damage> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Damage::Field(name)),
        }
    }
}

fn decode(bytes: &[u8]) -> Result<State, Damage> {
    if bytes.len() > MAX_FILE {
        return Err(Damage::Oversized);
    }
    let mut fields = Fields(bytes);
    if fields.take(MAGIC.len())? != MAGIC {
        return Err(Damage::Magic);
    }
    let length =
        usize::try_from(u32::from_be_bytes(fields.array()?)).map_err(|_| Damage::Oversized)?;
    let total = HEADER
        .checked_add(length)
        .and_then(|framed| framed.checked_add(DIGEST))
        .ok_or(Damage::Oversized)?;
    if bytes.len() < total {
        return Err(Damage::Truncated);
    }
    if bytes.len() > total {
        return Err(Damage::Trailing);
    }
    let (framed, digest) = bytes
        .split_at_checked(HEADER + length)
        .ok_or(Damage::Truncated)?;
    if crypto::digest(framed) != digest {
        return Err(Damage::Digest);
    }
    let mut body = Fields(framed.get(HEADER..).ok_or(Damage::Truncated)?);
    let pin = if body.flag("pin flag")? {
        let length = body.byte()?;
        Some(body.take(usize::from(length))?.to_vec())
    } else {
        None
    };
    let retries = body.byte()?;
    if retries > MAX_RETRIES {
        return Err(Damage::Field("retries"));
    }
    let counter = u32::from_be_bytes(body.array()?);
    let draws = u64::from_be_bytes(body.array()?);
    let length = body.byte()?;
    let seed = body.take(usize::from(length))?.to_vec();
    let count = usize::from(body.byte()?);
    if count > MAX_CREDENTIALS {
        return Err(Damage::Field("credentials"));
    }
    let mut credentials: Vec<Credential> = Vec::with_capacity(count);
    for _ in 0..count {
        let length = usize::from(u16::from_be_bytes(body.array()?));
        if !(1..=MAX_ID).contains(&length) {
            return Err(Damage::Field("credential ID"));
        }
        let id = body.take(length)?.to_vec();
        if credentials.iter().any(|credential| credential.id == id) {
            return Err(Damage::Field("duplicate credential"));
        }
        let private = body.array()?;
        if SecretScalar::from_bytes(Box::new(private)).is_err() {
            return Err(Damage::Field("credential key"));
        }
        let hmac = if body.flag("hmac-secret flag")? {
            Some((body.array()?, body.array()?))
        } else {
            None
        };
        let protect = body.byte()?;
        if !(1..=3).contains(&protect) {
            return Err(Damage::Field("credProtect"));
        }
        credentials.push(Credential {
            id,
            private,
            hmac,
            protect,
        });
    }
    if !body.0.is_empty() {
        return Err(Damage::Trailing);
    }
    Ok(State {
        pin,
        retries,
        counter,
        credentials,
        seed,
        draws,
    })
}

/// Where a test makes a persistent key's next save fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    /// Just before the rename: the file keeps the old state.
    Unpublished,
    /// Just after the rename, in place of the directory sync.
    Published,
}

/// A failed save: before the rename the file still holds the old state;
/// after it the name holds the new one, its durability unknown.
#[derive(Debug, PartialEq, Eq)]
enum Unsaved {
    Before(String),
    After(String),
}

/// Writes `file` whole and durably: a sibling `.next` file, synced, renamed
/// over it, and the directory synced.
fn save(file: &Path, state: &State, fault: Option<Fault>) -> Result<(), Unsaved> {
    let bytes = encode(state)
        .map_err(|damage| Unsaved::Before(format!("encode virtual key state: {damage}")))?;
    let directory = file
        .parent()
        .ok_or_else(|| Unsaved::Before("virtual key state names no directory".into()))?;
    let mut next = file.as_os_str().to_owned();
    next.push(".next");
    let next = PathBuf::from(next);
    let failed = |what: &'static str| {
        let file = file.display().to_string();
        move |error: std::io::Error| format!("{what} virtual key state {file}: {error}")
    };
    let mut out = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&next)
        .map_err(failed("create"))
        .map_err(Unsaved::Before)?;
    out.write_all(&bytes)
        .and_then(|()| out.sync_all())
        .map_err(failed("write and sync"))
        .map_err(Unsaved::Before)?;
    drop(out);
    if fault == Some(Fault::Unpublished) {
        return Err(Unsaved::Before("injected failure before the rename".into()));
    }
    // rename(2) either replaces the name or leaves it.
    fs::rename(&next, file)
        .map_err(failed("rename"))
        .map_err(Unsaved::Before)?;
    if fault == Some(Fault::Published) {
        return Err(Unsaved::After("injected failure after the rename".into()));
    }
    File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(failed("sync the directory of"))
        .map_err(Unsaved::After)
}

/// Reads at most one byte past the bound, so an oversized file is refused
/// without being read whole.
fn load(file: &Path) -> Result<State, Damage> {
    let mut bytes = Vec::new();
    File::open(file)
        .and_then(|opened| opened.take(MAX_FILE as u64 + 1).read_to_end(&mut bytes))
        .map_err(|error| Damage::Unreadable(format!("{}: {error}", file.display())))?;
    decode(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fido_ctap::signature;
    use crate::fido_pin::LoginRefusal;
    use crate::fido_pin::{EnrolledCredential, HmacOutput, Pin};
    use crate::fido_transaction::tests::fixture;
    use crate::fido_transaction::{
        Assertion, Enrollment, Error, LoginAssertion, LoginCreation, LoginError, LoginPin, Status,
        Transaction,
    };
    use crate::login_record::{NewKey, Record, VERSION};
    use std::time::Instant;

    const LABELS: &[&str] = &["p1-legacy", "p1-scoped", "p2-legacy", "p2-scoped"];
    const PIN: &[u8] = b"1234";
    const WRONG: &[u8] = b"4321";
    const SALT: [u8; 32] = [0x5a; 32];

    fn config(label: &str) -> Config {
        Config {
            protocols: if label.starts_with("p1") { &[1] } else { &[2] },
            permissions: label.ends_with("scoped"),
            token_length: if label == "p1-legacy" { 16 } else { 32 },
            ..Config::default()
        }
    }

    fn entropy() -> impl FnMut(&mut [u8]) -> Result<(), String> {
        let mut count = 0u32;
        move |out| {
            for chunk in out.chunks_mut(32) {
                count += 1;
                let bytes = crypto::digest(&count.to_be_bytes());
                chunk.copy_from_slice(&bytes[..chunk.len()]);
            }
            Ok(())
        }
    }

    fn pin(bytes: &[u8]) -> Result<Pin, String> {
        Pin::new(bytes.into())
    }

    fn key_of(cose: &[u8]) -> PublicKey {
        let value = cbor::decode(cose).unwrap();
        let coordinate = |key| -> [u8; 32] {
            value
                .required(&Value::Negative(key))
                .unwrap()
                .bytes()
                .unwrap()
                .try_into()
                .unwrap()
        };
        PublicKey::from_coordinates(&coordinate(1), &coordinate(2)).unwrap()
    }

    fn create(
        key: &Virtual,
        excluded: &[&[u8]],
        seed: u8,
    ) -> Result<EnrolledCredential, LoginError> {
        Transaction::new(key.link()).unwrap().login_create(
            LoginCreation {
                user: [seed ^ 0x55; 32],
                salt: SALT,
                excluded,
            },
            &mut |step, _| {
                let hash = match step {
                    LoginPin::Creation => [seed; 32],
                    LoginPin::Proof(_) => [seed ^ 0xaa; 32],
                };
                Ok((pin(PIN)?, hash))
            },
            &mut entropy(),
        )
    }

    /// A login assertion with this PIN; `shown` collects the retry counts prompted.
    fn login(
        key: &Virtual,
        credential: &EnrolledCredential,
        guess: &[u8],
        shown: &mut Vec<u8>,
    ) -> Result<HmacOutput, LoginError> {
        Transaction::new(key.link()).unwrap().login_assertion(
            LoginAssertion {
                credential: credential.id(),
                key: key_of(credential.cose()),
                salt: SALT,
            },
            &mut |_, count| {
                shown.push(count);
                Ok((pin(guess)?, [0x11; 32]))
            },
            &mut entropy(),
        )
    }

    /// `login` with the right PIN over this client-data hash.
    fn login_over(
        key: &Virtual,
        credential: &EnrolledCredential,
        hash: [u8; 32],
    ) -> Result<HmacOutput, LoginError> {
        Transaction::new(key.link()).unwrap().login_assertion(
            LoginAssertion {
                credential: credential.id(),
                key: key_of(credential.cose()),
                salt: SALT,
            },
            &mut |_, _| Ok((pin(PIN)?, hash)),
            &mut entropy(),
        )
    }

    fn identify(key: &Virtual, ids: &[&[u8]]) -> Result<Option<usize>, LoginError> {
        Transaction::new(key.link())
            .unwrap()
            .identify(ids, [0x22; 32])
    }

    fn failed(status: Status) -> LoginError {
        LoginError::Failed(Error::Status(status))
    }

    /// The commands after `start`, as (command, clientPIN subcommand) pairs.
    fn commands(key: &Virtual, start: usize) -> Vec<(u8, Option<u64>)> {
        key.transcript()[start..]
            .iter()
            .map(|(request, _)| {
                let sub = (request[0] == 6)
                    .then(|| unsigned(&cbor::decode(&request[1..]).unwrap(), 2).unwrap())
                    .flatten();
                (request[0], sub)
            })
            .collect()
    }

    /// The allow list of every getAssertion the key received after `start`.
    fn allow_lists(key: &Virtual, start: usize) -> Vec<Vec<Vec<u8>>> {
        key.transcript()[start..]
            .iter()
            .filter(|(request, _)| request[0] == 2)
            .map(|(request, _)| {
                let value = cbor::decode(&request[1..]).unwrap();
                let ids = descriptors(&value, 3).unwrap();
                ids.into_iter().map(<[u8]>::to_vec).collect()
            })
            .collect()
    }

    /// A raw platform: the authenticator's own protocol code on the other side.
    struct Platform {
        protocol: u64,
        private: SecretScalar,
        shared: Shared,
    }

    impl Platform {
        fn agree(key: &Virtual, protocol: u64) -> Self {
            let reply = key.exchange(&[6, 0xa2, 1, protocol as u8, 2, 2]);
            assert_eq!(reply[0], 0);
            let value = cbor::decode(&reply[1..]).unwrap();
            let peer = agreement_key(value.required(&Value::Unsigned(1)).unwrap()).unwrap();
            let private =
                SecretScalar::from_bytes(Box::new(fixture_scalar(&[protocol as u8; 32]))).unwrap();
            let shared = Shared::derive(protocol, private.agree(&peer).unwrap().bytes());
            Self {
                protocol,
                private,
                shared,
            }
        }

        fn cose(&self, out: &mut Encoder) -> Result<(), String> {
            encode_key(out, 24, &self.private.public_key().unwrap())
        }

        /// getPinToken's raw status.
        fn pin_token(&self, key: &Virtual, guess: &[u8]) -> u8 {
            self.token(key, guess, None).err().unwrap_or(0)
        }

        /// The decrypted token: legacy getPinToken, or subcommand 9 with
        /// these permissions and RP.
        fn token(
            &self,
            key: &Virtual,
            guess: &[u8],
            scope: Option<(u64, Option<&str>)>,
        ) -> Result<Vec<u8>, u8> {
            let hash = self.shared.seal(&[3; 16], &crypto::digest(guess)[..16]);
            let rp = scope.and_then(|(_, rp)| rp);
            let request = build(|out| {
                out.head(5, 4 + u64::from(scope.is_some()) + u64::from(rp.is_some()))?;
                out.head(0, 1)?;
                out.head(0, self.protocol)?;
                out.head(0, 2)?;
                out.head(0, if scope.is_some() { 9 } else { 5 })?;
                out.head(0, 3)?;
                self.cose(out)?;
                out.head(0, 6)?;
                out.bytes(&hash)?;
                if let Some((permissions, _)) = scope {
                    out.head(0, 9)?;
                    out.head(0, permissions)?;
                }
                if let Some(rp) = rp {
                    out.head(0, 10)?;
                    out.text(rp)?;
                }
                Ok(())
            });
            let reply = key.exchange(&[&[6][..], &request].concat());
            if reply[0] != 0 {
                return Err(reply[0]);
            }
            let value = cbor::decode(&reply[1..]).unwrap();
            let sealed = value
                .required(&Value::Unsigned(2))
                .unwrap()
                .bytes()
                .unwrap();
            Ok(self.shared.open(sealed).unwrap())
        }

        fn options(out: &mut Encoder, key: u64, options: &[(&str, bool)]) -> Result<(), String> {
            if options.is_empty() {
                return Ok(());
            }
            out.head(0, key)?;
            out.head(5, options.len() as u64)?;
            for (name, value) in options {
                out.text(name)?;
                out.boolean(*value)?;
            }
            Ok(())
        }

        /// pinUvAuthParam and protocol under `token`, when there is one.
        fn authorization(
            &self,
            out: &mut Encoder,
            key: u64,
            token: Option<&[u8]>,
        ) -> Result<(), String> {
            let Some(token) = token else {
                return Ok(());
            };
            out.head(0, key)?;
            out.bytes(&authenticate(self.protocol, token, &[0x44; 32]))?;
            out.head(0, key + 1)?;
            out.head(0, self.protocol)
        }

        /// A plain getAssertion's raw reply; options in canonical order.
        fn assertion(
            &self,
            key: &Virtual,
            rp: &str,
            credential: &[u8],
            token: Option<&[u8]>,
            options: &[(&str, bool)],
        ) -> Vec<u8> {
            let request = build(|out| {
                let count = 3 + u64::from(!options.is_empty()) + 2 * u64::from(token.is_some());
                out.head(5, count)?;
                out.head(0, 1)?;
                out.text(rp)?;
                out.head(0, 2)?;
                out.bytes(&[0x44; 32])?;
                out.head(0, 3)?;
                out.head(4, 1)?;
                out.head(5, 2)?;
                out.text("id")?;
                out.bytes(credential)?;
                out.text("type")?;
                out.text("public-key")?;
                Self::options(out, 5, options)?;
                self.authorization(out, 6, token)
            });
            key.exchange(&[&[2][..], &request].concat())
        }

        /// A minimal makeCredential's raw reply; options in canonical order.
        fn make(&self, key: &Virtual, token: Option<&[u8]>, options: &[(&str, bool)]) -> Vec<u8> {
            self.make_excluding(key, token, options, &[])
        }

        fn make_excluding(
            &self,
            key: &Virtual,
            token: Option<&[u8]>,
            options: &[(&str, bool)],
            excluded: &[&[u8]],
        ) -> Vec<u8> {
            let request = build(|out| {
                let count = 4
                    + u64::from(!excluded.is_empty())
                    + u64::from(!options.is_empty())
                    + 2 * u64::from(token.is_some());
                out.head(5, count)?;
                out.head(0, 1)?;
                out.bytes(&[0x44; 32])?;
                out.head(0, 2)?;
                out.head(5, 1)?;
                out.text("id")?;
                out.text(RP_ID)?;
                out.head(0, 3)?;
                out.head(5, 1)?;
                out.text("id")?;
                out.bytes(&[7; 16])?;
                out.head(0, 4)?;
                out.head(4, 1)?;
                out.head(5, 2)?;
                out.text("alg")?;
                out.head(1, 6)?;
                out.text("type")?;
                out.text("public-key")?;
                if !excluded.is_empty() {
                    out.head(0, 5)?;
                    out.head(4, excluded.len() as u64)?;
                    for id in excluded {
                        out.head(5, 2)?;
                        out.text("id")?;
                        out.bytes(id)?;
                        out.text("type")?;
                        out.text("public-key")?;
                    }
                }
                Self::options(out, 7, options)?;
                self.authorization(out, 8, token)
            });
            key.exchange(&[&[1][..], &request].concat())
        }

        /// hmac-secret without PIN authorization: up only, so no UV.
        fn unverified_secret(&self, key: &Virtual, credential: &[u8]) -> Vec<u8> {
            let salt_enc = self.shared.seal(&[4; 16], &SALT);
            let request = build(|out| {
                out.head(5, 5)?;
                out.head(0, 1)?;
                out.text(RP_ID)?;
                out.head(0, 2)?;
                out.bytes(&[0x33; 32])?;
                out.head(0, 3)?;
                out.head(4, 1)?;
                out.head(5, 2)?;
                out.text("id")?;
                out.bytes(credential)?;
                out.text("type")?;
                out.text("public-key")?;
                out.head(0, 4)?;
                out.head(5, 1)?;
                out.text("hmac-secret")?;
                out.head(5, 3 + u64::from(self.protocol == 2))?;
                out.head(0, 1)?;
                self.cose(out)?;
                out.head(0, 2)?;
                out.bytes(&salt_enc)?;
                out.head(0, 3)?;
                out.bytes(&self.shared.tag(&salt_enc))?;
                if self.protocol == 2 {
                    out.head(0, 4)?;
                    out.head(0, 2)?;
                }
                out.head(0, 5)?;
                out.head(5, 1)?;
                out.text("up")?;
                out.boolean(true)
            });
            let reply = key.exchange(&[&[2][..], &request].concat());
            assert_eq!(reply[0], 0);
            let value = cbor::decode(&reply[1..]).unwrap();
            let data = value
                .required(&Value::Unsigned(2))
                .unwrap()
                .bytes()
                .unwrap();
            assert_eq!(data[32], 0x81, "UP and ED, without UV");
            let extensions = cbor::decode(&data[37..]).unwrap();
            let sealed = extensions
                .required(&Value::Text("hmac-secret"))
                .unwrap()
                .bytes()
                .unwrap();
            self.shared.open(sealed).unwrap()
        }
    }

    #[test]
    fn signer_reproduces_every_vector_signature_and_verifies() {
        assert_eq!(fixture_scalar(&[0; 32])[31], 1);
        let mut below = fixture("p2-scoped", "signing");
        below.fill(0xff);
        // 2^256 - 1 reduces by n - 1 once.
        let reduced = fixture_scalar(&below.try_into().unwrap());
        assert!(SecretScalar::from_bytes(Box::new(reduced)).is_ok());
        for label in LABELS {
            let private: [u8; 32] = fixture(label, "signing").try_into().unwrap();
            let key = SecretScalar::from_bytes(Box::new(private)).unwrap();
            let public = key.public_key().unwrap();
            assert_eq!(
                public.coordinates(),
                (
                    fixture(label, "x").try_into().unwrap(),
                    fixture(label, "y").try_into().unwrap()
                )
            );
            let peer: [u8; 32] = fixture(label, "peer").try_into().unwrap();
            let scalar: [u8; 32] = fixture(label, "scalar").try_into().unwrap();
            let platform = SecretScalar::from_bytes(Box::new(scalar))
                .unwrap()
                .public_key()
                .unwrap();
            let shared = SecretScalar::from_bytes(Box::new(peer))
                .unwrap()
                .agree(&platform)
                .unwrap();
            assert_eq!(shared.bytes().as_slice(), fixture(label, "shared"));
            let mut count = 0;
            for name in [
                "response",
                "no_uv",
                "no_up",
                "missing",
                "wrong_extension",
                "short_output",
                "wrong_rp",
                "enroll_response",
                "enroll_be",
                "enroll_bs",
                "enroll_no_uv",
                "enroll_missing",
                "enroll_short",
                "make_packed",
            ] {
                let response = fixture(label, name);
                let value = cbor::decode(&response[1..]).unwrap();
                let data = value
                    .required(&Value::Unsigned(2))
                    .unwrap()
                    .bytes()
                    .unwrap();
                let (expected, bound) = if name == "make_packed" {
                    let statement = value.required(&Value::Unsigned(3)).unwrap();
                    (
                        statement.required(&Value::Text("sig")).unwrap(),
                        fixture(label, "create_challenge"),
                    )
                } else {
                    (
                        value.required(&Value::Unsigned(3)).unwrap(),
                        fixture(label, "challenge"),
                    )
                };
                let der = sign(&key, data, &bound);
                assert_eq!(der, expected.bytes().unwrap(), "{label} {name}");
                let (r, s) = signature(&der).unwrap();
                let digest = crypto::digest(&[data, &bound].concat());
                public.verify(&digest, &r, &s).unwrap();
                count += 1;
            }
            assert_eq!(count, 14);
        }
    }

    #[test]
    fn authenticator_decrypts_vector_pin_requests_and_reproduces_its_replies() {
        for label in LABELS {
            let two = label.starts_with("p2");
            let key = Virtual::new(config(label), Some(&fixture(label, "pin")), label);
            key.inject(fixture(label, "peer"));
            assert_eq!(
                key.exchange(&fixture(label, "key_request")),
                fixture(label, "key_response")
            );
            key.inject(fixture(label, "token"));
            if two {
                key.inject(fixture(label, "iv_token"));
            }
            assert_eq!(
                key.exchange(&fixture(label, "pin_request")),
                fixture(label, "pin_response"),
                "{label}"
            );
            assert_eq!(key.state().retries, 8);
            key.with_state(|state| {
                state.counter = 6;
                state.credentials.push(Credential {
                    id: b"fixture-id".to_vec(),
                    private: fixture(label, "signing").try_into().unwrap(),
                    hmac: Some(([1; 32], [2; 32])),
                    protect: 1,
                });
            });
            // Tampered authorization or salt authentication is refused.
            let assertion = fixture(label, "assertion");
            let value = cbor::decode(&assertion[1..]).unwrap();
            let param = value
                .required(&Value::Unsigned(6))
                .unwrap()
                .bytes()
                .unwrap();
            let auth = value
                .required(&Value::Unsigned(4))
                .unwrap()
                .required(&Value::Text("hmac-secret"))
                .unwrap()
                .required(&Value::Unsigned(3))
                .unwrap()
                .bytes()
                .unwrap();
            let [param, auth] =
                [param, auth].map(|field| field.as_ptr() as usize - assertion.as_ptr() as usize);
            let tamper = |request: &[u8], offset: usize| {
                let mut bad = request.to_vec();
                bad[offset] ^= 1;
                bad
            };
            let mut silent = assertion.clone();
            let up = silent.windows(4).rposition(|w| w == b"\x62up\xf5").unwrap();
            silent[up + 3] = 0xf4;
            // A bad pinUvAuthParam is refused first, with or without presence.
            for request in [&assertion, &silent] {
                let bad = tamper(request, param);
                assert_eq!(key.exchange(&bad), [PIN_AUTH_INVALID], "{label}");
            }
            // hmac-secret without presence is refused in extension processing;
            // nothing collected presence, so the token survives.
            assert_eq!(key.exchange(&silent), [UNSUPPORTED_OPTION], "{label}");
            // A bad saltAuth fails after presence, which spent the token.
            let bad = tamper(&assertion, auth);
            assert_eq!(key.exchange(&bad), [PIN_AUTH_INVALID], "{label}");
            assert_eq!(key.exchange(&assertion), [PIN_AUTH_INVALID], "{label}");
            assert_eq!(key.state().counter, 6);
            // A new token over the same key agreement.
            assert_eq!(
                key.exchange(&fixture(label, "key_request")),
                fixture(label, "key_response")
            );
            key.inject(fixture(label, "token"));
            if two {
                key.inject(fixture(label, "iv_token"));
            }
            assert_eq!(
                key.exchange(&fixture(label, "pin_request")),
                fixture(label, "pin_response")
            );
            key.script(Script {
                output: Output::Fixed(fixture(label, "output").try_into().unwrap()),
                ..Script::default()
            });
            if two {
                key.inject(fixture(label, "iv_output"));
            }
            assert_eq!(
                key.exchange(&assertion),
                fixture(label, "response"),
                "{label}"
            );
            // The token served that one assertion: a replay is refused.
            assert_eq!(key.exchange(&assertion), [PIN_AUTH_INVALID], "{label}");

            // The same request under another PIN fails its hash check.
            let other = Virtual::new(config(label), Some(b"other PIN"), label);
            for (retries, status) in [(7, PIN_INVALID), (6, PIN_INVALID), (5, PIN_AUTH_BLOCKED)] {
                other.inject(fixture(label, "peer"));
                other.exchange(&fixture(label, "key_request"));
                assert_eq!(other.exchange(&fixture(label, "pin_request")), [status]);
                assert_eq!(other.state().retries, retries);
            }
        }
    }

    #[test]
    fn production_flows_complete_for_both_protocols_and_token_kinds() {
        for label in LABELS {
            let key = Virtual::new(config(label), Some(PIN), label);
            let token = if label.ends_with("scoped") { 9 } else { 5 };
            let enrolled = Transaction::new(key.link())
                .unwrap()
                .enroll(
                    Enrollment {
                        challenge: [1; 32],
                        user: [2; 32],
                        proof_challenge: [3; 32],
                        salt: SALT,
                        excluded: &[],
                    },
                    &mut |_| pin(PIN),
                    &mut entropy(),
                )
                .unwrap();
            let state = key.state();
            assert_eq!(state.credentials.len(), 1);
            assert_eq!(enrolled.id(), state.credentials[0].id);
            assert_eq!(enrolled.id().len(), 64);
            let output = Transaction::new(key.link())
                .unwrap()
                .assertion(
                    Assertion {
                        credential: enrolled.id(),
                        key: key_of(enrolled.cose()),
                        challenge: [4; 32],
                        salt: SALT,
                    },
                    &mut |_| pin(PIN),
                    &mut entropy(),
                )
                .unwrap();
            assert_eq!(output.bytes(), enrolled.output().bytes());
            assert_eq!(
                commands(&key, 0),
                [
                    (4, None),
                    (6, Some(2)),
                    (6, Some(token)),
                    (1, None),
                    (6, Some(2)),
                    (6, Some(token)),
                    (2, None),
                    (4, None),
                    (6, Some(2)),
                    (6, Some(token)),
                    (2, None)
                ]
            );

            let start = key.transcript().len();
            let created = create(&key, &[], 5).unwrap();
            assert_eq!(
                commands(&key, start),
                [
                    (4, None),
                    (6, Some(2)),
                    (6, Some(1)),
                    (6, Some(token)),
                    (1, None),
                    (6, Some(2)),
                    (6, Some(1)),
                    (6, Some(token)),
                    (2, None)
                ]
            );
            let foreign = [0xf0; 64];
            assert_eq!(
                identify(&key, &[&foreign, created.id()]),
                Ok(Some(1)),
                "{label}"
            );
            for _ in 0..2 {
                let mut shown = Vec::new();
                let output = login(&key, &created, PIN, &mut shown).unwrap();
                assert_eq!(output.bytes(), created.output().bytes());
                assert!(output.info.user_verified);
                assert_eq!(shown, [8]);
            }
            // Each operation took its own token; replaying the last request fails.
            let (last, reply) = key.transcript().pop().unwrap();
            assert_eq!((last[0], reply[0]), (2, 0));
            assert_eq!(key.exchange(&last), [PIN_AUTH_INVALID], "{label}");
        }
    }

    #[test]
    fn tokens_are_rp_bound_and_single_use_and_options_follow_ctap_2_1() {
        let key = Virtual::new(config("p2-scoped"), Some(PIN), "tokens");
        let created = create(&key, &[], 1).unwrap();
        let id = created.id();
        let platform = Platform::agree(&key, 2);
        let token = |scope| platform.token(&key, PIN, scope).unwrap();
        // An mc or ga token must name its RP; no PIN attempt is spent.
        for permission in [MAKE_CREDENTIAL, GET_ASSERTION, 3] {
            assert_eq!(
                platform.token(&key, PIN, Some((permission, None))),
                Err(MISSING_PARAMETER)
            );
        }
        assert_eq!(
            platform.token(&key, PIN, Some((0x20, Some(RP_ID)))),
            Err(UNAUTHORIZED_PERMISSION)
        );
        assert_eq!(key.state().retries, 8);
        // Another RP's token, or one without the permission, authorizes nothing.
        let other = token(Some((GET_ASSERTION, Some("example.com"))));
        assert_eq!(
            platform.assertion(&key, RP_ID, id, Some(&other), &[]),
            [PIN_AUTH_INVALID]
        );
        let assertion = token(Some((GET_ASSERTION, Some(RP_ID))));
        assert_eq!(
            platform.make(&key, Some(&assertion), &[]),
            [PIN_AUTH_INVALID]
        );
        // Each token serves one request, whichever subcommand issued it.
        assert_eq!(
            platform.assertion(&key, RP_ID, id, Some(&assertion), &[])[0],
            0
        );
        assert_eq!(
            platform.assertion(&key, RP_ID, id, Some(&assertion), &[]),
            [PIN_AUTH_INVALID]
        );
        // Presence spends a token even without pinUvAuthParam.
        let assertion = token(Some((GET_ASSERTION, Some(RP_ID))));
        assert_eq!(platform.assertion(&key, RP_ID, id, None, &[])[0], 0);
        assert_eq!(
            platform.assertion(&key, RP_ID, id, Some(&assertion), &[]),
            [PIN_AUTH_INVALID]
        );
        // An authorized request without presence does not.
        let assertion = token(Some((GET_ASSERTION, Some(RP_ID))));
        for _ in 0..2 {
            let reply = platform.assertion(&key, RP_ID, id, Some(&assertion), &[("up", false)]);
            assert_eq!(reply[0], 0);
        }
        assert_eq!(
            platform.assertion(&key, RP_ID, id, Some(&assertion), &[])[0],
            0
        );
        assert_eq!(
            platform.assertion(&key, RP_ID, id, Some(&assertion), &[]),
            [PIN_AUTH_INVALID]
        );
        // Undefined permission bits are ignored.
        let assertion = token(Some((0x40 | GET_ASSERTION, Some(RP_ID))));
        assert_eq!(
            platform.assertion(&key, RP_ID, id, Some(&assertion), &[])[0],
            0
        );
        for permission in [0x04, 0x08, 0x10] {
            assert_eq!(
                platform.token(&key, PIN, Some((permission | GET_ASSERTION, Some(RP_ID)))),
                Err(UNAUTHORIZED_PERMISSION)
            );
        }
        // A legacy token takes the RP of its first authorized use, even when
        // that use finds no credential and so collects no presence.
        let legacy = token(None);
        assert_eq!(
            platform.assertion(&key, "example.com", id, Some(&legacy), &[]),
            [NO_CREDENTIALS]
        );
        assert_eq!(
            platform.assertion(&key, RP_ID, id, Some(&legacy), &[]),
            [PIN_AUTH_INVALID]
        );
        // The excludeList's presence precedes the presence step: the token
        // survives CREDENTIAL_EXCLUDED, granted, denied or timed out.
        for presence in [Presence::Granted, Presence::Denied, Presence::Timeout] {
            key.script(Script {
                presence,
                ..Script::default()
            });
            let creation = token(Some((MAKE_CREDENTIAL, Some(RP_ID))));
            assert_eq!(
                platform.make_excluding(&key, Some(&creation), &[], &[id]),
                [CREDENTIAL_EXCLUDED]
            );
            key.script(Script::default());
            assert_eq!(platform.make(&key, Some(&creation), &[])[0], 0);
            assert_eq!(
                platform.make(&key, Some(&creation), &[]),
                [PIN_AUTH_INVALID]
            );
        }
        let creation = token(Some((MAKE_CREDENTIAL, Some(RP_ID))));
        assert_eq!(platform.make(&key, Some(&creation), &[])[0], 0);
        assert_eq!(
            platform.make(&key, Some(&creation), &[]),
            [PIN_AUTH_INVALID]
        );
        let legacy = token(None);
        assert_eq!(platform.make(&key, Some(&legacy), &[])[0], 0);
        assert_eq!(platform.make(&key, Some(&legacy), &[]), [PIN_AUTH_INVALID]);
        let legacy = token(None);
        assert_eq!(
            platform.assertion(&key, RP_ID, id, Some(&legacy), &[])[0],
            0
        );
        assert_eq!(
            platform.assertion(&key, RP_ID, id, Some(&legacy), &[]),
            [PIN_AUTH_INVALID]
        );

        // Options: no built-in UV and no resident keys; uv is ignored beside
        // pinUvAuthParam.
        let both = token(Some((MAKE_CREDENTIAL | GET_ASSERTION, Some(RP_ID))));
        assert_eq!(
            platform.assertion(&key, RP_ID, id, Some(&both), &[("uv", true)])[0],
            0
        );
        assert_eq!(
            platform.make(&key, Some(&both), &[("uv", true)]),
            [PIN_AUTH_INVALID]
        );
        let both = token(Some((MAKE_CREDENTIAL, Some(RP_ID))));
        assert_eq!(platform.make(&key, Some(&both), &[("uv", true)])[0], 0);
        for (options, status) in [
            (&[("uv", true)][..], INVALID_OPTION),
            (&[("rk", false)], UNSUPPORTED_OPTION),
            (&[("rk", true)], UNSUPPORTED_OPTION),
            (&[("rk", true), ("uv", true)], INVALID_OPTION),
        ] {
            assert_eq!(platform.assertion(&key, RP_ID, id, None, options), [status]);
        }
        for (options, status) in [
            (&[("uv", true)][..], INVALID_OPTION),
            (&[("rk", true), ("uv", true)], INVALID_OPTION),
            (&[("up", false)], INVALID_OPTION),
            (&[("rk", true)], UNSUPPORTED_OPTION),
            (&[("rk", false)], PUAT_REQUIRED),
            (&[], PUAT_REQUIRED),
        ] {
            assert_eq!(platform.make(&key, None, options), [status]);
        }
        // alwaysUv demands pinUvAuthParam only where presence is asked for.
        let strict = Virtual::new(
            Config {
                always_uv: Some(true),
                ..Config::default()
            },
            Some(PIN),
            "strict",
        );
        strict.with_state(|state| {
            state.credentials.push(Credential {
                id: id.to_vec(),
                private: fixture_scalar(&[9; 32]),
                hmac: None,
                protect: 1,
            })
        });
        let raw = Platform::agree(&strict, 2);
        assert_eq!(
            raw.assertion(&strict, RP_ID, id, None, &[("up", false)])[0],
            0
        );
        assert_eq!(
            raw.assertion(&strict, RP_ID, id, None, &[]),
            [PUAT_REQUIRED]
        );
    }

    #[test]
    fn a_ctap_2_0_pin_token_stays_valid_until_replaced() {
        let key = Virtual::new(
            Config {
                versions: &["FIDO_2_0"],
                permissions: false,
                protocols: &[1],
                token_length: 16,
                ..Config::default()
            },
            Some(PIN),
            "legacy",
        );
        let created = create(&key, &[], 1).unwrap();
        for _ in 0..2 {
            let output = login(&key, &created, PIN, &mut Vec::new()).unwrap();
            assert_eq!(output.bytes(), created.output().bytes());
        }
        let platform = Platform::agree(&key, 1);
        let token = platform.token(&key, PIN, None).unwrap();
        for _ in 0..2 {
            assert_eq!(platform.make(&key, Some(&token), &[])[0], 0);
            assert_eq!(
                platform.assertion(&key, RP_ID, created.id(), Some(&token), &[])[0],
                0
            );
        }
        let newer = platform.token(&key, PIN, None).unwrap();
        assert_eq!(platform.make(&key, Some(&token), &[]), [PIN_AUTH_INVALID]);
        assert_eq!(platform.make(&key, Some(&newer), &[])[0], 0);
    }

    #[test]
    fn hmac_secret_is_stable_with_uv_and_differs_without() {
        for (label, protocol) in [("p1-scoped", 1), ("p2-scoped", 2)] {
            let key = Virtual::new(config(label), Some(PIN), label);
            let created = create(&key, &[], 7).unwrap();
            let first = login(&key, &created, PIN, &mut Vec::new()).unwrap();
            let second = login(&key, &created, PIN, &mut Vec::new()).unwrap();
            assert_eq!(first.bytes(), second.bytes());
            assert_eq!(first.bytes(), created.output().bytes());
            let platform = Platform::agree(&key, protocol);
            let unverified = platform.unverified_secret(&key, created.id());
            assert_eq!(unverified, platform.unverified_secret(&key, created.id()));
            assert_ne!(unverified, first.bytes());
            let (with_uv, without_uv) = key.state().credentials[0].hmac.unwrap();
            assert_eq!(first.bytes(), crypto::hmac(&with_uv, &SALT));
            assert_eq!(unverified, crypto::hmac(&without_uv, &SALT));
        }
    }

    #[test]
    fn identify_selects_across_batches_and_finds_nothing_for_foreign_ids() {
        let one = Virtual::new(
            Config {
                max_list: Some(2),
                ..Config::default()
            },
            Some(PIN),
            "one",
        );
        let other = Virtual::new(Config::default(), Some(PIN), "other");
        let mine = create(&one, &[], 1).unwrap();
        let theirs = create(&other, &[], 2).unwrap();
        let foreign = [[0xf0; 64], [0xf1; 64], [0xf2; 64]];
        let ids: &[&[u8]] = &[
            &foreign[0],
            theirs.id(),
            &foreign[1],
            &foreign[2],
            mine.id(),
        ];
        let start = one.transcript().len();
        assert_eq!(identify(&one, ids), Ok(Some(4)));
        let silent: Vec<_> = one.transcript()[start..]
            .iter()
            .filter(|(request, _)| request[0] == 2)
            .map(|(_, reply)| reply.clone())
            .collect();
        assert_eq!(silent.len(), 3);
        assert_eq!(silent[..2], [vec![NO_CREDENTIALS], vec![NO_CREDENTIALS]]);
        assert_eq!(identify(&other, ids), Ok(Some(1)));
        assert_eq!(identify(&one, &ids[..4]), Ok(None));
        // An absent list limit means one ID per batch.
        let unlisted = Virtual::new(
            Config {
                max_list: None,
                ..Config::default()
            },
            Some(PIN),
            "unlisted",
        );
        let only = create(&unlisted, &[], 3).unwrap();
        let start = unlisted.transcript().len();
        assert_eq!(
            identify(&unlisted, &[&foreign[0], &foreign[1], only.id()]),
            Ok(Some(2))
        );
        assert_eq!(commands(&unlisted, start).len(), 4);
        // The key itself refuses a list beyond its advertised limit.
        let request = crate::fido_ctap::IdentifyRequest::new(&ids[..3], [0x22; 32], 1024).unwrap();
        assert_eq!(one.exchange(request.bytes()), [LIMIT_EXCEEDED]);
    }

    #[test]
    fn default_cred_protect_hides_a_new_key_from_the_probe() {
        for (level, found) in [(3, None), (2, Some(0))] {
            let key = Virtual::new(
                Config {
                    extensions: Some(&["credProtect", "hmac-secret"]),
                    cred_protect: Some(level),
                    ..Config::default()
                },
                Some(PIN),
                "protected",
            );
            let created = create(&key, &[], 4).unwrap();
            assert_eq!(identify(&key, &[created.id()]), Ok(found));
            let output = login(&key, &created, PIN, &mut Vec::new()).unwrap();
            assert_eq!(output.bytes(), created.output().bytes());
        }
    }

    #[test]
    fn retries_block_after_three_failures_until_power_cycle_and_at_zero_for_good() {
        let label = "p2-scoped";
        let key = Virtual::new(config(label), Some(PIN), label);
        let created = create(&key, &[], 9).unwrap();
        let attempt = |guess: &[u8]| {
            let mut shown = Vec::new();
            let result = login(&key, &created, guess, &mut shown).err();
            (result, shown, key.state().retries)
        };
        let invalid = || Some(failed(Status::PinInvalid));
        let auth_blocked = || Some(failed(Status::PinAuthBlocked));
        let blocked = || Some(failed(Status::PinBlocked));
        assert_eq!(attempt(WRONG), (invalid(), vec![8], 7));
        assert_eq!(attempt(WRONG), (invalid(), vec![7], 6));
        // A correct PIN restores the count and the consecutive allowance.
        assert_eq!(attempt(PIN), (None, vec![6], 8));
        assert_eq!(attempt(WRONG), (invalid(), vec![8], 7));
        assert_eq!(attempt(WRONG), (invalid(), vec![7], 6));
        assert_eq!(attempt(WRONG), (auth_blocked(), vec![6], 5));
        // Blocked until power cycle: no prompt, and the key refuses even the
        // right PIN without spending a retry.
        assert_eq!(attempt(PIN), (auth_blocked(), vec![], 5));
        assert_eq!(
            Platform::agree(&key, 2).pin_token(&key, PIN),
            PIN_AUTH_BLOCKED
        );
        assert_eq!(key.state().retries, 5);
        key.power_cycle();
        assert_eq!(attempt(WRONG), (invalid(), vec![5], 4));
        assert_eq!(attempt(WRONG), (invalid(), vec![4], 3));
        assert_eq!(attempt(WRONG), (auth_blocked(), vec![3], 2));
        key.power_cycle();
        assert_eq!(attempt(WRONG), (invalid(), vec![2], 1));
        assert_eq!(attempt(WRONG), (blocked(), vec![1], 0));
        assert_eq!(attempt(PIN), (blocked(), vec![], 0));
        key.power_cycle();
        assert_eq!(attempt(PIN), (blocked(), vec![], 0));
        assert_eq!(Platform::agree(&key, 2).pin_token(&key, PIN), PIN_BLOCKED);
        assert_eq!(key.state().retries, 0);
    }

    #[test]
    fn exclusion_list_returns_credential_excluded_and_creates_nothing() {
        let key = Virtual::new(Config::default(), Some(PIN), "excluded");
        let first = create(&key, &[], 1).unwrap();
        let before = key.state().credentials;
        assert_eq!(
            create(&key, &[first.id()], 2).err(),
            Some(failed(Status::CredentialExcluded))
        );
        assert_eq!(
            Transaction::new(key.link())
                .unwrap()
                .enroll(
                    Enrollment {
                        challenge: [3; 32],
                        user: [4; 32],
                        proof_challenge: [5; 32],
                        salt: SALT,
                        excluded: &[first.id()],
                    },
                    &mut |_| pin(PIN),
                    &mut entropy(),
                )
                .err(),
            Some(Error::Status(Status::CredentialExcluded))
        );
        assert_eq!(key.state().credentials, before);
        let foreign = [0xf0; 64];
        assert!(create(&key, &[&foreign], 6).is_ok());
        assert_eq!(key.state().credentials.len(), 2);
    }

    #[test]
    fn get_info_variants_select_typed_admission() {
        let refused = |config: Config, pin_set: bool| {
            let key = Virtual::new(config, pin_set.then_some(PIN), "info");
            create(&key, &[], 1).err()
        };
        let refusal = |reason| Some(LoginError::Refused(reason));
        for extensions in [None, Some(&[][..]), Some(&["credProtect"][..])] {
            let config = Config {
                extensions,
                ..Config::default()
            };
            assert_eq!(refused(config, true), refusal(LoginRefusal::NoHmacSecret));
        }
        let always = |always_uv| Config {
            always_uv,
            ..Config::default()
        };
        assert_eq!(
            refused(always(Some(true)), true),
            refusal(LoginRefusal::AlwaysUv)
        );
        assert_eq!(refused(always(Some(false)), true), None);
        assert_eq!(
            refused(Config::default(), false),
            refusal(LoginRefusal::PinNotSet)
        );
        let unsupported = Config {
            pin_support: false,
            ..Config::default()
        };
        assert_eq!(
            refused(unsupported, false),
            refusal(LoginRefusal::PinUnsupported)
        );
        let unversioned = Config {
            versions: &["U2F_V2"],
            ..Config::default()
        };
        assert!(matches!(
            refused(unversioned, true),
            Some(LoginError::Failed(Error::Protocol(_)))
        ));
        let foreign: &[&[u8]] = &[&[0xf0; 64], &[0xf1; 64]];
        let unlisted = Virtual::new(
            Config {
                max_list: None,
                ..Config::default()
            },
            Some(PIN),
            "unlisted",
        );
        assert_eq!(
            create(&unlisted, foreign, 1).err(),
            refusal(LoginRefusal::ListTooSmall)
        );
        // Both protocols advertised: the client chooses protocol 2.
        let both = Virtual::new(Config::default(), Some(PIN), "both");
        create(&both, &[], 1).unwrap();
        assert!(both
            .transcript()
            .iter()
            .filter(|(request, _)| request[0] == 6)
            .all(|(request, _)| request[2..4] == [1, 2]));
        // A short ID limit: the client never sends a longer ID.
        let short = Virtual::new(
            Config {
                max_id: Some(48),
                id_length: 48,
                ..Config::default()
            },
            Some(PIN),
            "short",
        );
        let created = create(&short, &[], 2).unwrap();
        let start = short.transcript().len();
        assert_eq!(identify(&short, &[&[0xf0; 64], created.id()]), Ok(Some(1)));
        assert_eq!(commands(&short, start).len(), 2);
        assert_eq!(allow_lists(&short, start), [vec![created.id().to_vec()]]);
        // Packed self attestation is admitted like none.
        let packed = Virtual::new(
            Config {
                attestation: Attestation::Packed,
                ..Config::default()
            },
            Some(PIN),
            "packed",
        );
        assert!(create(&packed, &[], 3).is_ok());
    }

    #[test]
    fn scripted_presence_output_signature_and_backup_faults() {
        let key = Virtual::new(Config::default(), Some(PIN), "script");
        let created = create(&key, &[], 1).unwrap();
        for (presence, status) in [
            (Presence::Denied, Status::Denied),
            (Presence::Timeout, Status::TouchTimeout),
        ] {
            key.script(Script {
                presence,
                ..Script::default()
            });
            assert_eq!(create(&key, &[], 2).err(), Some(failed(status)));
            assert_eq!(
                login(&key, &created, PIN, &mut Vec::new()).err(),
                Some(failed(status))
            );
        }
        let delay = Duration::from_millis(20);
        key.script(Script {
            presence: Presence::Delayed(delay),
            ..Script::default()
        });
        let started = Instant::now();
        assert!(login(&key, &created, PIN, &mut Vec::new()).is_ok());
        assert!(started.elapsed() >= delay);
        // A held touch waits for its release, and a release is used once.
        key.script(Script {
            presence: Presence::Held,
            ..Script::default()
        });
        let releaser = key.clone();
        let touched = key.touched();
        let held = std::thread::spawn(move || {
            while !releaser.touching() {
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(delay);
            assert!(releaser.touching());
            releaser.release_touch();
        });
        assert!(login(&key, &created, PIN, &mut Vec::new()).is_ok());
        held.join().unwrap();
        assert!(!key.touching());
        assert!(key.touched() >= touched + delay);

        // A wrong secret still verifies its signature; the record refuses it.
        let output: [u8; 32] = created.output().bytes().try_into().unwrap();
        let record = Record::enroll(
            1000,
            [7; 32],
            VERSION,
            vec![NewKey {
                credential: created.id().to_vec(),
                key: key_of(created.cose()),
                salt: SALT,
                output: &output,
            }],
        )
        .unwrap();
        key.script(Script {
            output: Output::Wrong,
            ..Script::default()
        });
        let wrong = login(&key, &created, PIN, &mut Vec::new()).unwrap();
        let wrong: [u8; 32] = wrong.bytes().try_into().unwrap();
        assert_ne!(wrong, output);
        assert!(!record.check(created.id(), &wrong).unwrap());
        assert!(record.check(created.id(), &output).unwrap());

        let mismatch = Some(LoginError::Failed(Error::Protocol(
            "portable assertion signature mismatch".into(),
        )));
        for signing in [Signing::Stale, Signing::Foreign] {
            key.script(Script::default());
            assert!(login(&key, &created, PIN, &mut Vec::new()).is_ok());
            key.script(Script {
                signing,
                ..Script::default()
            });
            assert_eq!(login(&key, &created, PIN, &mut Vec::new()).err(), mismatch);
        }
        // A replay signs the current data over the last presence hash: it
        // verifies only when the client sends that hash again.
        key.script(Script::default());
        assert!(login_over(&key, &created, [0x31; 32]).is_ok());
        key.script(Script {
            signing: Signing::Replayed,
            ..Script::default()
        });
        assert_eq!(login_over(&key, &created, [0x32; 32]).err(), mismatch);
        assert!(login_over(&key, &created, [0x32; 32]).is_ok());
        key.script(Script::default());

        for backup in [0x08, 0x18] {
            key.script(Script {
                backup,
                ..Script::default()
            });
            assert_eq!(
                create(&key, &[], 3).err(),
                Some(LoginError::Failed(Error::Protocol(
                    "portable enrollment requires UP, UV, AT, ED and a device-bound key".into()
                )))
            );
            assert_eq!(
                login(&key, &created, PIN, &mut Vec::new()).err(),
                Some(LoginError::Failed(Error::Protocol(
                    "login assertion is not device-bound".into()
                )))
            );
        }
    }

    /// A private directory for state files, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("td-virtual-key-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A file with this body, framed and digested as `encode` frames it.
    fn framed(body: &[u8]) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend((body.len() as u32).to_be_bytes());
        bytes.extend(body);
        let digest = crypto::digest(&bytes);
        bytes.extend(digest);
        bytes
    }

    fn body(bytes: &[u8]) -> Vec<u8> {
        bytes[HEADER..bytes.len() - DIGEST].to_vec()
    }

    #[test]
    fn a_restored_key_keeps_credentials_secrets_pin_and_retries_but_not_volatile_state() {
        let scratch = Scratch::new("restore");
        let file = scratch.0.join("key");
        let key = Virtual::persistent(Config::default(), Some(PIN), "restore", &file).unwrap();
        assert_eq!(
            Virtual::restore(Config::default(), &file).unwrap().state(),
            key.state()
        );
        // Never over an existing key.
        assert!(Virtual::persistent(Config::default(), Some(PIN), "again", &file).is_err());
        let created = create(&key, &[], 4).unwrap();
        let before = login(&key, &created, PIN, &mut Vec::new()).unwrap();
        // Every change is on disk before its reply.
        assert_eq!(load(&file).unwrap(), key.state());
        let mut shown = Vec::new();
        for _ in 0..3 {
            assert!(login(&key, &created, WRONG, &mut shown).is_err());
        }
        assert_eq!(shown, [8, 7, 6]);
        // PIN AUTH BLOCKED is volatile: a cold boot is a power cycle.
        assert!(login(&key, &created, PIN, &mut Vec::new()).is_err());
        let state = key.state();
        drop(key);
        let restored = Virtual::restore(Config::default(), &file).unwrap();
        assert_eq!(restored.state(), state);
        assert_eq!(restored.state().retries, 5);
        let mut shown = Vec::new();
        let after = login(&restored, &created, PIN, &mut shown).unwrap();
        assert_eq!(shown, [5]);
        assert_eq!(after.bytes(), before.bytes());
        // The signature counter goes on from where it was saved.
        assert!(restored.state().counter > state.counter);
        assert_eq!(load(&file).unwrap(), restored.state());
        // A test's edit is saved too.
        restored.with_state(|state| state.retries = 2);
        assert_eq!(load(&file).unwrap().retries, 2);
        assert!(restored.saved().is_ok());
        assert_eq!(fs::read_dir(&scratch.0).unwrap().count(), 1);
    }

    #[test]
    fn a_save_that_fails_answers_other_and_keeps_the_state_it_had() {
        let scratch = Scratch::new("unsaved");
        let file = scratch.0.join("key");
        let key = Virtual::persistent(Config::default(), Some(PIN), "unsaved", &file).unwrap();
        let state = key.state();
        fs::remove_dir_all(&scratch.0).unwrap();
        assert_eq!(
            create(&key, &[], 5).err(),
            Some(failed(Status::Other(OTHER)))
        );
        assert_eq!(key.state(), state);
        assert!(key.saved().unwrap_err().contains("virtual key state"));
        // A request that changes nothing saves nothing and still answers.
        assert_eq!(key.exchange(&[4])[0], 0);
    }

    /// getKeyAgreement for protocol 2, which draws the agreement key.
    const AGREE: &[u8] = &[6, 0xa2, 1, 2, 2, 2];

    #[test]
    fn a_save_failing_before_its_rename_undoes_the_state_and_the_key_goes_on() {
        let scratch = Scratch::new("unpublished");
        let file = scratch.0.join("key");
        let key = Virtual::persistent(Config::default(), Some(PIN), "unpublished", &file).unwrap();
        let state = key.state();
        key.fail_next_save(Fault::Unpublished);
        assert_eq!(key.exchange(AGREE), [OTHER]);
        assert_eq!(key.state(), state);
        assert_eq!(load(&file).unwrap(), state);
        assert!(key.saved().unwrap_err().contains("before the rename"));
        // The agreement key went with its rewound draw: it is drawn again,
        // once, and saved.
        assert_eq!(key.exchange(AGREE)[0], 0);
        assert_eq!(key.state().draws, state.draws + 1);
        assert_eq!(load(&file).unwrap(), key.state());
    }

    #[test]
    fn a_save_failing_after_its_rename_poisons_the_key() {
        let scratch = Scratch::new("published");
        let file = scratch.0.join("key");
        let key = Virtual::persistent(Config::default(), Some(PIN), "published", &file).unwrap();
        let before = key.state();
        key.fail_next_save(Fault::Published);
        assert_eq!(key.exchange(AGREE), [OTHER]);
        // The name holds the new state, and so does the key.
        let state = key.state();
        assert_ne!(state, before);
        assert_eq!(load(&file).unwrap(), state);
        // Every further request and edit is refused and changes nothing.
        for request in [&[4][..], AGREE, &[6, 0xa2, 1, 2, 2, 1]] {
            assert_eq!(key.exchange(request), [OTHER]);
        }
        key.with_state(|state| state.retries = 1);
        assert_eq!(key.state(), state);
        assert_eq!(load(&file).unwrap(), state);
        assert!(key
            .saved()
            .unwrap_err()
            .contains("the key refuses every request"));
        assert_eq!(
            Virtual::restore(Config::default(), &file).unwrap().state(),
            state
        );
    }

    static ALARMS: AtomicU64 = AtomicU64::new(0);

    fn alarm(_: &str) {
        ALARMS.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn a_frozen_key_refuses_every_save_before_writing_and_raises_its_alarm() {
        let scratch = Scratch::new("frozen");
        let file = scratch.0.join("key");
        let key = Virtual::persistent(Config::default(), Some(PIN), "frozen", &file).unwrap();
        let (state, bytes) = (key.state(), fs::read(&file).unwrap());
        key.freeze(alarm);
        assert_eq!(key.exchange(AGREE), [OTHER]);
        assert_eq!(ALARMS.load(Ordering::SeqCst), 1);
        assert_eq!(key.state(), state);
        assert_eq!(fs::read(&file).unwrap(), bytes);
        assert_eq!(fs::read_dir(&scratch.0).unwrap().count(), 1);
        assert!(key.saved().unwrap_err().contains("being cut"));
        // A request that changes nothing writes nothing and still answers.
        assert_eq!(key.exchange(&[4])[0], 0);
        assert_eq!(ALARMS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn damaged_state_files_are_refused_and_never_panic() {
        let key = Virtual::new(Config::default(), Some(PIN), "damage");
        create(&key, &[], 6).unwrap();
        create(&key, &[], 7).unwrap();
        let state = key.state();
        let bytes = encode(&state).unwrap();
        assert_eq!(decode(&bytes).unwrap(), state);
        assert_eq!(framed(&body(&bytes)), bytes);
        // Every truncation, every flipped bit and a trailing byte.
        for length in 0..bytes.len() {
            assert_eq!(decode(&bytes[..length]), Err(Damage::Truncated), "{length}");
        }
        for index in 0..bytes.len() {
            for bit in 0..8 {
                let mut flipped = bytes.clone();
                flipped[index] ^= 1 << bit;
                assert!(decode(&flipped).is_err(), "{index} {bit}");
            }
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(decode(&trailing), Err(Damage::Trailing));
        let mut magic = bytes.clone();
        magic[7] = b'2';
        assert_eq!(decode(&magic), Err(Damage::Magic));
        let mut oversized = bytes.clone();
        oversized.resize(MAX_FILE + 1, 0);
        assert_eq!(decode(&oversized), Err(Damage::Oversized));
        let mut declared = bytes.clone();
        declared[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(decode(&declared), Err(Damage::Truncated));

        // Files whose digest holds but whose fields are out of bounds: the
        // PIN, retries, counter, draws and seed, then the credentials.
        let body = body(&bytes);
        let retries = 2 + PIN.len();
        let count = retries + 1 + 4 + 8 + 1 + state.seed.len();
        let first = count + 1;
        let private = first + 2 + state.credentials[0].id.len();
        let hmac = private + 32;
        let protect = hmac + 1 + 64;
        let edits: &[(usize, &[u8], Damage)] = &[
            (0, &[2], Damage::Field("pin flag")),
            (retries, &[MAX_RETRIES + 1], Damage::Field("retries")),
            (
                count,
                &[MAX_CREDENTIALS as u8 + 1],
                Damage::Field("credentials"),
            ),
            (first, &[0, 0], Damage::Field("credential ID")),
            (first, &[4, 1], Damage::Field("credential ID")),
            (private, &[0xff; 32], Damage::Field("credential key")),
            (private, &[0; 32], Damage::Field("credential key")),
            (hmac, &[2], Damage::Field("hmac-secret flag")),
            (protect, &[0], Damage::Field("credProtect")),
            (protect, &[4], Damage::Field("credProtect")),
        ];
        for (at, edit, damage) in edits {
            let mut changed = body.clone();
            changed[*at..*at + edit.len()].copy_from_slice(edit);
            assert_eq!(decode(&framed(&changed)), Err(damage.clone()), "{at}");
        }
        // The first credential twice.
        let second = protect + 1;
        let mut duplicate = body.clone();
        duplicate.splice(second..second, body[first..second].to_vec());
        duplicate[count] = 3;
        assert_eq!(
            decode(&framed(&duplicate)),
            Err(Damage::Field("duplicate credential"))
        );
        // A body longer than its fields, and one that runs out.
        let mut long = body.clone();
        long.push(0);
        assert_eq!(decode(&framed(&long)), Err(Damage::Trailing));
        assert_eq!(
            decode(&framed(&body[..body.len() - 1])),
            Err(Damage::Truncated)
        );
        // A state outside the bounds never encodes.
        let mut wide = state.clone();
        wide.seed = vec![0; 256];
        assert_eq!(encode(&wide), Err(Damage::Field("seed")));
        let mut spent = state;
        spent.retries = MAX_RETRIES + 1;
        assert_eq!(encode(&spent), Err(Damage::Field("retries")));
        assert!(matches!(
            load(Path::new("/nonexistent/td-virtual-key")),
            Err(Damage::Unreadable(_))
        ));
    }
}
