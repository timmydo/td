//! The disk protector's FIDO2 flows (td-install/ENCRYPTION.md, "FIDO2
//! protectors"): identify, the PIN-authorized hmac-secret assertion with
//! its signature verified under the token's key, the keyslot passphrase,
//! and creation with its proof, repeat and both probes. Each runs one
//! transaction per session over `Devices`, on exactly one connected key,
//! under the login profile's admission (`FIDO_2_1` included). The caller
//! owns the header, its description, the console and the keyslot.

use super::crypto;
use super::fido_ctap::Es256PublicKey;
use super::fido_device::Interruption;
use super::fido_p256::PublicKey;
use super::fido_pin::{HmacOutput, LoginRefusal, Pin};
use super::fido_transaction::{
    Assertion, Devices, Error, LoginAssertion, LoginCreation, LoginError, LoginPin, Transaction,
};
use std::time::Instant;

/// The client-data domain, before its zero byte.
const OPERATION: &[u8] = b"td/disk-protector/operation/v1";
/// The keyslot passphrase's HKDF info, before its zero byte.
const PASSPHRASE: &[u8] = b"td/disk-protector/fido2/v1";
/// A fido2 token's longest credential ID, which bounds the header.
pub const MAX_CREDENTIAL: usize = 255;
/// The most fido2 tokens a header holds ("Protectors and shapes").
pub const MAX_TOKENS: usize = 4;

/// What an assertion authorizes: the client-data phase byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Unlock = 1,
    Create = 2,
    Prove = 3,
    Repeat = 4,
    Probe = 5,
    Authorize = 6,
}

/// SHA-256 over the domain, a zero byte, the phase, the u32 big-endian
/// length-prefixed canonical description and 32 fresh random bytes.
pub fn client_data_hash(
    phase: Phase,
    description: &[u8],
    random: &[u8; 32],
) -> Result<[u8; 32], String> {
    let size = u32::try_from(description.len()).map_err(|_| "disk description length overflow")?;
    let mut input = Vec::with_capacity(OPERATION.len() + 6 + description.len() + 32);
    input.extend_from_slice(OPERATION);
    input.push(0);
    input.push(phase as u8);
    input.extend_from_slice(&size.to_be_bytes());
    input.extend_from_slice(description);
    input.extend_from_slice(random);
    Ok(crypto::digest(&input))
}

/// A keyslot passphrase, zeroed on drop. It reaches cryptsetup as a key
/// file, never argv.
pub struct Passphrase(Box<[u8; 32]>);
impl Passphrase {
    pub fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}
impl Drop for Passphrase {
    fn drop(&mut self) {
        crypto::zero(self.0.as_mut());
    }
}

/// HKDF-SHA256 with the hmac-secret output as input keying material, an
/// empty salt, and the info `td/disk-protector/fido2/v1`, a zero byte, the
/// volume's 16-byte UUID and the u32 big-endian length-prefixed credential.
pub fn passphrase(
    output: &[u8],
    volume: &[u8; 16],
    credential: &[u8],
) -> Result<Passphrase, String> {
    if output.len() != 32 {
        return Err("hmac-secret output is not 32 bytes".into());
    }
    if credential.is_empty() || credential.len() > MAX_CREDENTIAL {
        return Err("disk credential ID length".into());
    }
    let size = u32::try_from(credential.len()).map_err(|_| "disk credential length")?;
    let mut out = Passphrase(Box::new([0; 32]));
    crypto::hkdf_expand(
        output,
        &[],
        &[PASSPHRASE, &[0], volume, &size.to_be_bytes(), credential],
        out.0.as_mut(),
    )?;
    Ok(out)
}

/// A header fido2 token's public half.
pub struct Token<'a> {
    pub credential: &'a [u8],
    pub key: PublicKey,
    pub salt: [u8; 32],
}

/// What every hash of one operation binds: its canonical description.
#[derive(Clone, Copy)]
pub struct Operation<'a> {
    pub volume: [u8; 16],
    pub description: &'a [u8],
}

/// The person at the console or screen.
pub trait Person {
    /// A PIN step, with the key's own retry count, an untrusted claim. An
    /// error ends the operation before the PIN reaches the key.
    fn pin(&mut self, phase: Phase, retries: u8) -> Result<Pin, String>;
    /// The key will next wait for a touch: told once each PIN is entered,
    /// as ENCRYPTION.md's console prints its touch line after the PIN, and
    /// before the no-PIN probe, which asks none. The transaction has no
    /// step between the key's PIN check and its presence wait; a wrong PIN
    /// ends the step with its status instead.
    fn touch(&mut self, phase: Phase);
}

#[derive(Debug, PartialEq, Eq)]
pub enum DiskError {
    /// None or several FIDO devices, or another device than this
    /// operation's first session used.
    OneKey,
    /// The key holds none of the tokens' credentials.
    NotEnrolled,
    /// A credential ID empty or longer than 255 bytes: a token's, before
    /// any token I/O, or a new credential's, before its proof's PIN.
    CredentialLength,
    /// The repeat did not reproduce the proof's output.
    Mismatch,
    /// The new credential's identify probe did not select it.
    Unselectable,
    /// Admission's typed refusals, `NoFido21` and the no-PIN probe's
    /// `PinOptional` among them.
    Refused(LoginRefusal),
    /// The person ended a PIN step, or a stepped creation's probe step.
    Ended,
    /// A typed CTAP status, an interruption (`Expired` past the deadline),
    /// the transport, entropy, or a protocol or verification failure such
    /// as a signature under another key.
    Failed(Error),
}

impl From<LoginError> for DiskError {
    fn from(error: LoginError) -> Self {
        match error {
            LoginError::Refused(refusal) => Self::Refused(refusal),
            LoginError::Failed(Error::PinInput) => Self::Ended,
            LoginError::Failed(error) => Self::Failed(error),
        }
    }
}

/// An unlock's or authorization's result: which token, and its passphrase.
pub struct Released {
    pub token: usize,
    pub passphrase: Passphrase,
}

/// Identify the connected key's credential among `tokens` silently, then
/// its PIN-authorized assertion: `phase` is `Unlock`, or `Authorize` for
/// protector management. The assertion is verified under the token's key
/// (RP hash, UP, UV and the signature) before its output is decrypted.
pub fn release<D: Devices>(
    devices: &mut D,
    deadline: Instant,
    tokens: &[Token<'_>],
    operation: Operation<'_>,
    phase: Phase,
    person: &mut impl Person,
    entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
) -> Result<Released, DiskError> {
    if !matches!(phase, Phase::Unlock | Phase::Authorize) {
        return Err(DiskError::Failed(Error::Protocol(
            "a release is an unlock or an authorization".into(),
        )));
    }
    selected(
        devices,
        deadline,
        tokens,
        &operation.volume,
        Identified {
            phase,
            description: operation.description,
        },
        &mut |_, retries, random| {
            let pin = person.pin(phase, retries)?;
            person.touch(phase);
            Ok((pin, client_data_hash(phase, operation.description, random)?))
        },
        entropy,
    )
}

/// The steps of an authorization presented one at a time
/// (td-authd/DESIGN.md amendment 10, a login operation's disk proof): its
/// assertion's description names the identified token and the key's PIN
/// retries, so it exists only once the key has answered.
pub trait Steps {
    /// The authorize step for `token`, an index into the release's tokens,
    /// with the key's own retry count, an untrusted claim: the step's
    /// canonical description, once presented, and the PIN typed for it. An
    /// error ends the operation before the PIN reaches the key.
    fn authorize(&mut self, token: usize, retries: u8) -> Result<(Pin, Vec<u8>), String>;
}

/// `release` under the authorize phase in two presented steps: the
/// identify hash binds `identify`, the identify step's description, and
/// the assertion's binds the description `steps` gives for the selected
/// token. Every check is `release`'s.
pub fn authorize<D: Devices>(
    devices: &mut D,
    deadline: Instant,
    tokens: &[Token<'_>],
    volume: &[u8; 16],
    identify: &[u8],
    steps: &mut impl Steps,
    entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
) -> Result<Released, DiskError> {
    selected(
        devices,
        deadline,
        tokens,
        volume,
        Identified {
            phase: Phase::Authorize,
            description: identify,
        },
        &mut |token, retries, random| {
            let (pin, description) = steps.authorize(token, retries)?;
            Ok((
                pin,
                client_data_hash(Phase::Authorize, &description, random)?,
            ))
        },
        entropy,
    )
}

/// What the silent identify's hash binds.
struct Identified<'a> {
    phase: Phase,
    description: &'a [u8],
}

/// The PIN step for the selected token and the key's retries, under the
/// assertion's fresh random bytes: the PIN and the client-data hash.
type Answer<'a> = dyn FnMut(usize, u8, &[u8; 32]) -> Result<(Pin, [u8; 32]), String> + 'a;

fn selected<D: Devices>(
    devices: &mut D,
    deadline: Instant,
    tokens: &[Token<'_>],
    volume: &[u8; 16],
    identified: Identified<'_>,
    answer: &mut Answer<'_>,
    entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
) -> Result<Released, DiskError> {
    if tokens.is_empty() || tokens.len() > MAX_TOKENS {
        return Err(DiskError::Failed(Error::Protocol(
            "a release names one to four tokens".into(),
        )));
    }
    if tokens
        .iter()
        .any(|token| token.credential.is_empty() || token.credential.len() > MAX_CREDENTIAL)
    {
        return Err(DiskError::CredentialLength);
    }
    let node = one(devices)?;
    let ids: Vec<&[u8]> = tokens.iter().map(|token| token.credential).collect();
    let hash = client_data_hash(identified.phase, identified.description, &fresh(entropy)?)
        .map_err(protocol)?;
    let selected = Transaction::new(open(devices, node, deadline)?)
        .map_err(LoginError::from)
        .and_then(|transaction| transaction.identify(&ids, hash))?;
    let index = selected.ok_or(DiskError::NotEnrolled)?;
    let token = tokens
        .get(index)
        .ok_or_else(|| protocol("identify extent"))?;
    same_key(devices, node)?;
    let random = fresh(entropy)?;
    let (x, y) = token.key.coordinates();
    let key = PublicKey::from_coordinates(&x, &y).map_err(protocol)?;
    let output = Transaction::new(open(devices, node, deadline)?)
        .map_err(LoginError::from)
        .and_then(|transaction| {
            transaction.login_assertion(
                LoginAssertion {
                    credential: token.credential,
                    key,
                    salt: token.salt,
                },
                &mut |_, retries| answer(index, retries, &random),
                entropy,
            )
        })?;
    let passphrase = passphrase(output.bytes(), volume, token.credential).map_err(protocol)?;
    Ok(Released {
        token: index,
        passphrase,
    })
}

/// A new fido2 token's credential, salt and passphrase.
pub struct Created {
    pub credential: Vec<u8>,
    /// The credential's public COSE key, as makeCredential returned it.
    pub cose: Vec<u8>,
    pub salt: [u8; 32],
    pub passphrase: Passphrase,
}

/// A new credential under the `td disk` labels on the one connected key,
/// excluding `excluded` (every credential the header's fido2 tokens and
/// the operation already name, in header order), each step on its own
/// session: creation and its proof, each with its PIN and touch; a repeat
/// that must reproduce the proof's output exactly; the identify probe over
/// `excluded` then the new credential, which must select it; and the
/// no-PIN probe, which must not give the PIN's output. Every phase's hash
/// binds the operation's one description.
pub fn create<D: Devices>(
    devices: &mut D,
    deadline: Instant,
    excluded: &[&[u8]],
    operation: Operation<'_>,
    person: &mut impl Person,
    entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
) -> Result<Created, DiskError> {
    created(
        devices,
        deadline,
        excluded,
        &operation.volume,
        &mut Described {
            description: operation.description,
            person,
        },
        entropy,
    )
}

/// The steps of a creation presented one at a time (td-authd/DESIGN.md
/// amendment 10, the upgrade's key ceremony): a step's description names
/// the key's PIN retries or the new credential, so it exists only once
/// the key has answered.
pub trait CreateSteps {
    /// A PIN step with the key's own retry count, an untrusted claim: the
    /// step's canonical description, once presented, and the PIN typed for
    /// it. An error ends the operation before the PIN reaches the key.
    fn pin(&mut self, step: CreateStep<'_>, retries: u8) -> Result<(Pin, Vec<u8>), String>;
    /// The probe step for the new credential: its description, once
    /// presented, which both probes bind. An error ends the operation.
    fn probe(&mut self, credential: &[u8]) -> Result<Vec<u8>, String>;
    /// The key will next wait for a touch, as `Person::touch`.
    fn touch(&mut self, phase: Phase);
}

/// A stepped creation's PIN steps: create, before any credential
/// exists, then prove and repeat for the new one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CreateStep<'a> {
    Create,
    Prove(&'a [u8]),
    Repeat(&'a [u8]),
}

impl<'a> CreateStep<'a> {
    pub fn phase(self) -> Phase {
        match self {
            Self::Create => Phase::Create,
            Self::Prove(_) => Phase::Prove,
            Self::Repeat(_) => Phase::Repeat,
        }
    }

    /// The new credential a prove or repeat step names.
    pub fn credential(self) -> Option<&'a [u8]> {
        match self {
            Self::Create => None,
            Self::Prove(credential) | Self::Repeat(credential) => Some(credential),
        }
    }
}

/// `create` in presented steps: each phase's hash binds the description
/// `steps` gives for it, the identify and no-PIN probes the probe step's.
/// Every check is `create`'s.
pub fn create_stepped<D: Devices>(
    devices: &mut D,
    deadline: Instant,
    excluded: &[&[u8]],
    volume: &[u8; 16],
    steps: &mut impl CreateSteps,
    entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
) -> Result<Created, DiskError> {
    created(devices, deadline, excluded, volume, steps, entropy)
}

/// One description for every phase of `create`.
struct Described<'a, P> {
    description: &'a [u8],
    person: &'a mut P,
}

impl<P: Person> CreateSteps for Described<'_, P> {
    fn pin(&mut self, step: CreateStep<'_>, retries: u8) -> Result<(Pin, Vec<u8>), String> {
        Ok((
            self.person.pin(step.phase(), retries)?,
            self.description.to_vec(),
        ))
    }
    fn probe(&mut self, _: &[u8]) -> Result<Vec<u8>, String> {
        Ok(self.description.to_vec())
    }
    fn touch(&mut self, phase: Phase) {
        self.person.touch(phase);
    }
}

fn created<D: Devices>(
    devices: &mut D,
    deadline: Instant,
    excluded: &[&[u8]],
    volume: &[u8; 16],
    steps: &mut impl CreateSteps,
    entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
) -> Result<Created, DiskError> {
    if excluded.len() >= MAX_TOKENS {
        return Err(protocol("a header holds at most four fido2 tokens"));
    }
    if excluded
        .iter()
        .any(|id| id.is_empty() || id.len() > MAX_CREDENTIAL)
    {
        return Err(DiskError::CredentialLength);
    }
    let node = one(devices)?;
    let user = fresh(entropy)?;
    let salt = fresh(entropy)?;
    let (create_random, prove_random) = (fresh(entropy)?, fresh(entropy)?);
    let mut long = false;
    let created = Transaction::new(open(devices, node, deadline)?)
        .map_err(LoginError::from)
        .and_then(|transaction| {
            transaction.disk_create(
                LoginCreation {
                    user,
                    salt,
                    excluded,
                },
                &mut |step, retries| {
                    let (step, random) = match step {
                        LoginPin::Creation => (CreateStep::Create, &create_random),
                        // The header's bound, before the proof's PIN.
                        LoginPin::Proof(credential) if credential.len() > MAX_CREDENTIAL => {
                            long = true;
                            return Err("disk credential longer than 255 bytes".into());
                        }
                        LoginPin::Proof(credential) => {
                            (CreateStep::Prove(credential), &prove_random)
                        }
                    };
                    let (pin, description) = steps.pin(step, retries)?;
                    let phase = step.phase();
                    steps.touch(phase);
                    Ok((pin, client_data_hash(phase, &description, random)?))
                },
                entropy,
            )
        });
    let created = match created {
        Err(LoginError::Failed(Error::PinInput)) if long => {
            return Err(DiskError::CredentialLength)
        }
        other => other?,
    };
    let credential = created.id();
    let (x, y) = key_of(created.cose())?.coordinates();
    let public = || PublicKey::from_coordinates(&x, &y).map_err(protocol);
    same_key(devices, node)?;
    let random = fresh(entropy)?;
    let repeated = Transaction::new(open(devices, node, deadline)?)
        .map_err(LoginError::from)
        .and_then(|transaction| {
            transaction.login_assertion(
                LoginAssertion {
                    credential,
                    key: public().map_err(|_| Error::Protocol("credential key".into()))?,
                    salt,
                },
                &mut |_, retries| {
                    let (pin, description) = steps.pin(CreateStep::Repeat(credential), retries)?;
                    steps.touch(Phase::Repeat);
                    Ok((pin, client_data_hash(Phase::Repeat, &description, &random)?))
                },
                entropy,
            )
        })?;
    let reproduced = crypto::same(repeated.bytes(), created.output().bytes());
    drop(repeated);
    if !reproduced {
        return Err(DiskError::Mismatch);
    }
    // After the person's wait, so the one-key check follows it.
    let description = steps.probe(credential).map_err(|_| DiskError::Ended)?;
    same_key(devices, node)?;
    let mut ids: Vec<&[u8]> = excluded.to_vec();
    ids.push(credential);
    let hash = client_data_hash(Phase::Probe, &description, &fresh(entropy)?).map_err(protocol)?;
    let selected = Transaction::new(open(devices, node, deadline)?)
        .map_err(LoginError::from)
        .and_then(|transaction| transaction.identify(&ids, hash))?;
    if selected != Some(excluded.len()) {
        return Err(DiskError::Unselectable);
    }
    same_key(devices, node)?;
    let hash = client_data_hash(Phase::Probe, &description, &fresh(entropy)?).map_err(protocol)?;
    let key = public()?;
    let channel = open(devices, node, deadline)?;
    steps.touch(Phase::Probe);
    probe(
        channel,
        credential,
        key,
        hash,
        salt,
        created.output(),
        entropy,
    )?;
    let passphrase = passphrase(created.output().bytes(), volume, credential).map_err(protocol)?;
    Ok(Created {
        credential: credential.to_vec(),
        cose: created.cose().to_vec(),
        salt,
        passphrase,
    })
}

fn probe<C: super::fido_transaction::Channel>(
    channel: C,
    credential: &[u8],
    key: PublicKey,
    challenge: [u8; 32],
    salt: [u8; 32],
    repeated: &HmacOutput,
    entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
) -> Result<(), DiskError> {
    Transaction::new(channel)
        .map_err(LoginError::from)
        .and_then(|transaction| {
            transaction.probe(
                Assertion {
                    credential,
                    key,
                    challenge,
                    salt,
                },
                repeated,
                entropy,
            )
        })
        .map_err(DiskError::from)
}

/// The P-256 key of makeCredential's public COSE map, under the codec
/// the enrollment proof's verification uses.
fn key_of(cose: &[u8]) -> Result<PublicKey, DiskError> {
    let key = Es256PublicKey::from_cose(cose).map_err(protocol)?;
    let (x, y) = key.coordinates();
    PublicKey::from_coordinates(x, y).map_err(protocol)
}

/// Exactly one connected device, refused before any PIN otherwise.
fn one<D: Devices>(devices: &mut D) -> Result<D::Node, DiskError> {
    match devices
        .discover()
        .map_err(|_| DiskError::Failed(Error::Transport))?
        .as_slice()
    {
        [node] => Ok(*node),
        _ => Err(DiskError::OneKey),
    }
}

/// Still exactly the device the operation began on.
fn same_key<D: Devices>(devices: &mut D, node: D::Node) -> Result<(), DiskError> {
    if one(devices)? != node {
        return Err(DiskError::OneKey);
    }
    Ok(())
}

/// A session under the operation's absolute deadline.
fn open<D: Devices>(
    devices: &mut D,
    node: D::Node,
    deadline: Instant,
) -> Result<D::Channel, DiskError> {
    if Instant::now() >= deadline {
        return Err(DiskError::Failed(Error::Interrupted(Interruption::Expired)));
    }
    devices
        .open(node, deadline)
        .map_err(|_| DiskError::Failed(Error::Transport))
}

fn fresh(entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>) -> Result<[u8; 32], DiskError> {
    let mut random = [0; 32];
    entropy(&mut random).map_err(|_| DiskError::Failed(Error::Entropy))?;
    Ok(random)
}

fn protocol(error: impl Into<String>) -> DiskError {
    DiskError::Failed(Error::Protocol(error.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fido_cbor::{self as cbor, Value};
    use crate::fido_transaction::Status;
    use crate::fido_virtual::{Config, Link, Output, Script, Signing, Virtual};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::time::Duration;

    const PIN: &[u8] = b"4321";
    const DESCRIPTION: &[u8] = b"unlock td volume 00112233-4455-6677-8899-aabbccddeeff";
    const VOLUME: [u8; 16] = [0x5c; 16];

    fn vector(group: &str, name: &str) -> Vec<u8> {
        let row = include_str!("../tests/disk_vectors.txt")
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
            .find(|row| row.first() == Some(&group) && row.get(1) == Some(&name))
            .unwrap();
        row[2]
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    /// Virtual keys, by node; which are connected before session N opens
    /// (the last repeating), and a script applied as session N opens.
    struct Keys {
        nodes: Vec<Virtual>,
        connected: Vec<Vec<usize>>,
        scripts: Vec<(usize, Script)>,
        opened: usize,
        /// Once set, every node is connected.
        all: Rc<Cell<bool>>,
    }

    impl Keys {
        fn one(key: &Virtual) -> Self {
            Self::sessions(&[key], vec![vec![0]])
        }
        fn sessions(nodes: &[&Virtual], connected: Vec<Vec<usize>>) -> Self {
            Self {
                nodes: nodes.iter().map(|key| (*key).clone()).collect(),
                connected,
                scripts: Vec::new(),
                opened: 0,
                all: Rc::default(),
            }
        }
    }

    impl Devices for Keys {
        type Node = usize;
        type Channel = Link;
        fn discover(&mut self) -> Result<Vec<usize>, String> {
            if self.all.get() {
                return Ok((0..self.nodes.len()).collect());
            }
            Ok(self
                .connected
                .get(self.opened)
                .or(self.connected.last())
                .cloned()
                .unwrap_or_default())
        }
        fn open(&mut self, node: usize, _: Instant) -> Result<Link, String> {
            for (at, script) in &self.scripts {
                if *at == self.opened {
                    self.nodes[node].script(*script);
                }
            }
            self.opened += 1;
            Ok(self.nodes[node].link())
        }
        fn pause(&mut self, _: Duration) {}
    }

    /// The console: its PIN steps and touch requests, in order, and the
    /// PIN it types; `ends` refuses that phase's PIN step.
    #[derive(Default)]
    struct Console {
        pins: Vec<(Phase, u8)>,
        touches: Vec<Phase>,
        pin: Option<&'static [u8]>,
        ends: Option<Phase>,
    }

    impl Person for Console {
        fn pin(&mut self, phase: Phase, retries: u8) -> Result<Pin, String> {
            self.pins.push((phase, retries));
            if self.ends == Some(phase) {
                return Err("the person ended it".into());
            }
            Pin::new(self.pin.unwrap_or(PIN).into())
        }
        fn touch(&mut self, phase: Phase) {
            self.touches.push(phase);
        }
    }

    /// Deterministic entropy that keeps every 32-byte draw.
    fn entropy(draws: &Rc<RefCell<Vec<[u8; 32]>>>) -> impl FnMut(&mut [u8]) -> Result<(), String> {
        let draws = Rc::clone(draws);
        let mut count = 0u32;
        move |out| {
            for chunk in out.chunks_mut(32) {
                count += 1;
                let bytes = crypto::digest(&[b"disk".as_slice(), &count.to_be_bytes()].concat());
                chunk.copy_from_slice(&bytes[..chunk.len()]);
            }
            if let Ok(draw) = <[u8; 32]>::try_from(&*out) {
                draws.borrow_mut().push(draw);
            }
            Ok(())
        }
    }

    fn operation() -> Operation<'static> {
        Operation {
            volume: VOLUME,
            description: DESCRIPTION,
        }
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    fn blank(seed: &str) -> Virtual {
        Virtual::new(Config::default(), Some(PIN), seed)
    }

    fn create_on(key: &Virtual, console: &mut Console) -> Result<Created, DiskError> {
        let draws = Rc::new(RefCell::new(Vec::new()));
        create(
            &mut Keys::one(key),
            later(),
            &[],
            operation(),
            console,
            &mut entropy(&draws),
        )
    }

    fn token(created: &Created) -> Token<'_> {
        Token {
            credential: &created.credential,
            key: key_of(&created.cose).unwrap(),
            salt: created.salt,
        }
    }

    fn release_on(
        keys: &mut Keys,
        tokens: &[Token<'_>],
        console: &mut Console,
    ) -> Result<Released, DiskError> {
        let draws = Rc::new(RefCell::new(Vec::new()));
        release(
            keys,
            later(),
            tokens,
            operation(),
            Phase::Unlock,
            console,
            &mut entropy(&draws),
        )
    }

    /// The client-data hash of every request with this command, in order.
    fn hashes(key: &Virtual, command: u8, member: u64) -> Vec<[u8; 32]> {
        key.transcript()
            .iter()
            .filter(|(request, _)| request[0] == command)
            .map(|(request, _)| {
                cbor::decode(&request[1..])
                    .unwrap()
                    .required(&Value::Unsigned(member))
                    .unwrap()
                    .bytes()
                    .unwrap()
                    .try_into()
                    .unwrap()
            })
            .collect()
    }

    fn binds(hash: &[u8; 32], phase: Phase, draws: &[[u8; 32]]) -> bool {
        draws
            .iter()
            .any(|draw| client_data_hash(phase, DESCRIPTION, draw).unwrap() == *hash)
    }

    #[test]
    fn hashes_and_passphrases_match_independent_vectors() {
        let description = vector("hash", "description");
        let random: [u8; 32] = vector("hash", "random").try_into().unwrap();
        for (name, phase) in [
            ("unlock", Phase::Unlock),
            ("create", Phase::Create),
            ("prove", Phase::Prove),
            ("repeat", Phase::Repeat),
            ("probe", Phase::Probe),
            ("authorize", Phase::Authorize),
        ] {
            assert_eq!(
                client_data_hash(phase, &description, &random).unwrap(),
                *vector("hash", name),
                "{name}"
            );
        }
        assert_eq!(
            client_data_hash(Phase::Unlock, &[], &random).unwrap(),
            *vector("hash", "empty")
        );
        let output = vector("passphrase", "output");
        let volume: [u8; 16] = vector("passphrase", "volume").try_into().unwrap();
        for name in ["one", "short", "longest"] {
            let credential = vector("passphrase", &format!("credential_{name}"));
            let derived = passphrase(&output, &volume, &credential).unwrap();
            assert_eq!(
                *derived.bytes(),
                *vector("passphrase", &format!("key_{name}"))
            );
        }
        // Every input is bound: another volume, credential or output.
        let short = vector("passphrase", "credential_short");
        let key = *passphrase(&output, &volume, &short).unwrap().bytes();
        assert_ne!(*passphrase(&output, &[0; 16], &short).unwrap().bytes(), key);
        assert_ne!(
            *passphrase(&output, &volume, b"disk-credentiaL")
                .unwrap()
                .bytes(),
            key
        );
        assert_ne!(*passphrase(&[0; 32], &volume, &short).unwrap().bytes(), key);
        for (output, credential) in [
            (&output[..31], &short[..]),
            (&output[..], &[][..]),
            (&output[..], &[7; 256][..]),
        ] {
            assert!(passphrase(output, &volume, credential).is_err());
        }
    }

    #[test]
    fn a_created_token_releases_its_passphrase_under_each_phase_hash() {
        let key = blank("disk-create");
        let draws = Rc::new(RefCell::new(Vec::new()));
        let mut keys = Keys::one(&key);
        let mut console = Console::default();
        let created = create(
            &mut keys,
            later(),
            &[],
            operation(),
            &mut console,
            &mut entropy(&draws),
        )
        .unwrap();
        // Create and prove on one session, the repeat and each probe on new ones.
        assert_eq!(keys.opened, 4);
        assert_eq!(
            console.pins,
            [(Phase::Create, 8), (Phase::Prove, 8), (Phase::Repeat, 8)]
        );
        assert_eq!(
            console.touches,
            [Phase::Create, Phase::Prove, Phase::Repeat, Phase::Probe]
        );
        assert_eq!(created.credential, key.state().credentials[0].id);
        // td disk labels, not the login's.
        let make = key
            .transcript()
            .into_iter()
            .find(|(request, _)| request[0] == 1)
            .unwrap()
            .0;
        let label = |text: &[u8]| make.windows(text.len()).any(|window| window == text);
        assert!(label(b"\x67td disk") && !label(b"td login"));
        // Each hash binds its phase and a fresh draw.
        let draws = draws.borrow().clone();
        let [create_hash] = hashes(&key, 1, 1)[..] else {
            panic!("one creation")
        };
        assert!(binds(&create_hash, Phase::Create, &draws));
        let assertions = hashes(&key, 2, 2);
        let [prove, repeat, probe, presence] = assertions[..] else {
            panic!("four assertions")
        };
        assert!(binds(&prove, Phase::Prove, &draws));
        assert!(binds(&repeat, Phase::Repeat, &draws));
        assert!(binds(&probe, Phase::Probe, &draws));
        assert!(binds(&presence, Phase::Probe, &draws));
        assert_ne!(probe, presence);
        // The passphrase is HKDF over the PIN's output for its salt.
        let (with_uv, _) = key.state().credentials[0].hmac.unwrap();
        let output = crypto::hmac(&with_uv, &created.salt);
        let expected = passphrase(&output, &VOLUME, &created.credential).unwrap();
        assert_eq!(created.passphrase.bytes(), expected.bytes());
        // A release, among the header's tokens, gives the same passphrase.
        let other = blank("disk-other");
        let foreign = create_on(&other, &mut Console::default()).unwrap();
        let tokens = [token(&foreign), token(&created)];
        for phase in [Phase::Unlock, Phase::Authorize] {
            let draws = Rc::new(RefCell::new(Vec::new()));
            let mut console = Console::default();
            let mut keys = Keys::one(&key);
            let released = release(
                &mut keys,
                later(),
                &tokens,
                operation(),
                phase,
                &mut console,
                &mut entropy(&draws),
            )
            .unwrap();
            assert_eq!(released.token, 1);
            assert_eq!(released.passphrase.bytes(), created.passphrase.bytes());
            assert_eq!(keys.opened, 2);
            assert_eq!(console.pins, [(phase, 8)]);
            assert_eq!(console.touches, [phase]);
            let assertions = hashes(&key, 2, 2);
            let [identify, assertion] = assertions[assertions.len() - 2..] else {
                panic!("two assertions")
            };
            let draws = draws.borrow().clone();
            assert!(binds(&identify, phase, &draws));
            assert!(binds(&assertion, phase, &draws));
        }
        assert!(matches!(
            release(
                &mut Keys::one(&key),
                later(),
                &tokens,
                operation(),
                Phase::Probe,
                &mut Console::default(),
                &mut |_| Ok(()),
            ),
            Err(DiskError::Failed(Error::Protocol(_)))
        ));
    }

    #[test]
    fn a_release_refuses_strangers_signatures_statuses_and_bounds() {
        let key = blank("disk-release");
        let created = create_on(&key, &mut Console::default()).unwrap();
        let second = create_on(&key, &mut Console::default()).unwrap();
        // A key holding none of the tokens: no PIN step.
        let stranger = blank("disk-stranger");
        let mut console = Console::default();
        assert_eq!(
            release_on(&mut Keys::one(&stranger), &[token(&created)], &mut console).err(),
            Some(DiskError::NotEnrolled)
        );
        assert!(console.pins.is_empty());
        // A signature under another key, and a token naming this credential
        // with the other credential's key: refused after the PIN, nothing released.
        key.script(Script {
            signing: Signing::Foreign,
            ..Script::default()
        });
        let mut console = Console::default();
        assert!(matches!(
            release_on(&mut Keys::one(&key), &[token(&created)], &mut console),
            Err(DiskError::Failed(Error::Protocol(_)))
        ));
        assert_eq!(console.pins, [(Phase::Unlock, 8)]);
        key.script(Script::default());
        let wrong = Token {
            key: key_of(&second.cose).unwrap(),
            ..token(&created)
        };
        assert!(matches!(
            release_on(&mut Keys::one(&key), &[wrong], &mut Console::default()),
            Err(DiskError::Failed(Error::Protocol(_)))
        ));
        // A wrong output for the right signature: a passphrase the keyslot refuses.
        key.script(Script {
            output: Output::Wrong,
            ..Script::default()
        });
        let released = release_on(
            &mut Keys::one(&key),
            &[token(&created)],
            &mut Console::default(),
        )
        .unwrap();
        assert_ne!(released.passphrase.bytes(), created.passphrase.bytes());
        key.script(Script::default());
        // Each PIN status stays typed; blocked keys get no PIN step.
        let mut console = Console {
            pin: Some(b"0000"),
            ..Console::default()
        };
        for retries in [8, 7] {
            assert_eq!(
                release_on(&mut Keys::one(&key), &[token(&created)], &mut console).err(),
                Some(DiskError::Failed(Error::Status(Status::PinInvalid))),
                "{retries}"
            );
        }
        assert_eq!(console.pins, [(Phase::Unlock, 8), (Phase::Unlock, 7)]);
        assert_eq!(
            release_on(&mut Keys::one(&key), &[token(&created)], &mut console).err(),
            Some(DiskError::Failed(Error::Status(Status::PinAuthBlocked)))
        );
        let mut fresh = Console::default();
        assert_eq!(
            release_on(&mut Keys::one(&key), &[token(&created)], &mut fresh).err(),
            Some(DiskError::Failed(Error::Status(Status::PinAuthBlocked)))
        );
        assert!(fresh.pins.is_empty());
        key.power_cycle();
        key.with_state(|state| state.retries = 0);
        assert_eq!(
            release_on(&mut Keys::one(&key), &[token(&created)], &mut fresh).err(),
            Some(DiskError::Failed(Error::Status(Status::PinBlocked)))
        );
        assert!(fresh.pins.is_empty());
        key.with_state(|state| state.retries = 8);
        // The person ending the PIN step.
        let mut ending = Console {
            ends: Some(Phase::Unlock),
            ..Console::default()
        };
        assert_eq!(
            release_on(&mut Keys::one(&key), &[token(&created)], &mut ending).err(),
            Some(DiskError::Ended)
        );
        // Bounds, before any token I/O: a 256-byte or empty credential,
        // five tokens.
        let start = key.transcript().len();
        for credential in [&[7; 256][..], &[]] {
            let bad = Token {
                credential,
                ..token(&created)
            };
            assert_eq!(
                release_on(&mut Keys::one(&key), &[bad], &mut Console::default()).err(),
                Some(DiskError::CredentialLength)
            );
        }
        let five: Vec<Token<'_>> = (0..5).map(|_| token(&created)).collect();
        assert!(matches!(
            release_on(&mut Keys::one(&key), &five, &mut Console::default()),
            Err(DiskError::Failed(Error::Protocol(_)))
        ));
        assert_eq!(key.transcript().len(), start);
        // CTAP 2.0 alone: refused at identify's getInfo, before any PIN.
        key.configure(|config| config.versions = &["FIDO_2_0"]);
        let mut console = Console::default();
        assert_eq!(
            release_on(&mut Keys::one(&key), &[token(&created)], &mut console).err(),
            Some(DiskError::Refused(LoginRefusal::NoFido21))
        );
        assert!(console.pins.is_empty());
        assert_eq!(key.transcript().len(), start + 1);
    }

    #[test]
    fn exactly_one_key_and_the_deadline_bound_every_session() {
        let (key, other) = (blank("disk-one"), blank("disk-two"));
        let created = create_on(&key, &mut Console::default()).unwrap();
        for connected in [vec![], vec![0, 1]] {
            let mut keys = Keys::sessions(&[&key, &other], vec![connected]);
            assert_eq!(
                release_on(&mut keys, &[token(&created)], &mut Console::default()).err(),
                Some(DiskError::OneKey)
            );
            assert_eq!(keys.opened, 0);
            let mut keys = Keys::sessions(&[&key, &other], vec![vec![]]);
            assert_eq!(
                create(
                    &mut keys,
                    later(),
                    &[],
                    operation(),
                    &mut Console::default(),
                    &mut |_| Ok(()),
                )
                .err(),
                Some(DiskError::OneKey)
            );
            assert_eq!(keys.opened, 0);
        }
        // Another device after identify: refused before any PIN.
        let mut keys = Keys::sessions(&[&key, &other], vec![vec![0], vec![1]]);
        let mut console = Console::default();
        assert_eq!(
            release_on(&mut keys, &[token(&created)], &mut console).err(),
            Some(DiskError::OneKey)
        );
        assert_eq!((keys.opened, console.pins.len()), (1, 0));
        // A deadline already passed opens nothing.
        let mut keys = Keys::one(&key);
        assert_eq!(
            release(
                &mut keys,
                Instant::now(),
                &[token(&created)],
                operation(),
                Phase::Unlock,
                &mut Console::default(),
                &mut |_| Ok(()),
            )
            .err(),
            Some(DiskError::Failed(Error::Interrupted(Interruption::Expired)))
        );
        assert_eq!(keys.opened, 0);
    }

    #[test]
    fn creation_refuses_keys_that_cannot_require_their_pin() {
        // CTAP 2.0 alone: refused at the first getInfo, before any PIN.
        let old = Virtual::new(
            Config {
                versions: &["FIDO_2_0"],
                ..Config::default()
            },
            Some(PIN),
            "disk-ctap-2-0",
        );
        let mut console = Console::default();
        assert_eq!(
            create_on(&old, &mut console).err(),
            Some(DiskError::Refused(LoginRefusal::NoFido21))
        );
        assert!(console.pins.is_empty());
        assert_eq!(old.transcript().len(), 1);
        assert!(old.state().credentials.is_empty());
        // FIDO_2_1 claimed over one secret: the no-PIN probe catches it.
        let one = Virtual::new(
            Config {
                one_secret: true,
                ..Config::default()
            },
            Some(PIN),
            "disk-one-secret",
        );
        let mut console = Console::default();
        assert_eq!(
            create_on(&one, &mut console).err(),
            Some(DiskError::Refused(LoginRefusal::PinOptional))
        );
        assert_eq!(console.pins.len(), 3);
        assert_eq!(console.touches.last(), Some(&Phase::Probe));
    }

    #[test]
    fn creation_refuses_mismatch_hidden_long_and_excluded_credentials() {
        // The repeat's session returns another output.
        let key = blank("disk-mismatch");
        let mut keys = Keys::one(&key);
        keys.scripts = vec![(
            1,
            Script {
                output: Output::Wrong,
                ..Script::default()
            },
        )];
        let draws = Rc::new(RefCell::new(Vec::new()));
        assert_eq!(
            create(
                &mut keys,
                later(),
                &[],
                operation(),
                &mut Console::default(),
                &mut entropy(&draws),
            )
            .err(),
            Some(DiskError::Mismatch)
        );
        assert_eq!(keys.opened, 2);
        // A default credProtect that hides the credential from a silent
        // assertion fails the identify probe.
        let hidden = Virtual::new(
            Config {
                cred_protect: Some(3),
                ..Config::default()
            },
            Some(PIN),
            "disk-hidden",
        );
        assert_eq!(
            create_on(&hidden, &mut Console::default()).err(),
            Some(DiskError::Unselectable)
        );
        // A 256-byte credential ID: refused before the proof's PIN.
        let long = Virtual::new(
            Config {
                id_length: 256,
                max_id: None,
                ..Config::default()
            },
            Some(PIN),
            "disk-long",
        );
        let mut console = Console::default();
        assert_eq!(
            create_on(&long, &mut console).err(),
            Some(DiskError::CredentialLength)
        );
        assert_eq!(console.pins, [(Phase::Create, 8)]);
        let fits = Virtual::new(
            Config {
                id_length: 255,
                max_id: None,
                ..Config::default()
            },
            Some(PIN),
            "disk-255",
        );
        assert_eq!(
            create_on(&fits, &mut Console::default())
                .unwrap()
                .credential
                .len(),
            255
        );
        // A key already holding an excluded credential.
        let key = blank("disk-excluded");
        let first = create_on(&key, &mut Console::default()).unwrap();
        let draws = Rc::new(RefCell::new(Vec::new()));
        let mut keys = Keys::one(&key);
        assert_eq!(
            create(
                &mut keys,
                later(),
                &[&first.credential],
                operation(),
                &mut Console::default(),
                &mut entropy(&draws),
            )
            .err(),
            Some(DiskError::Failed(Error::Status(Status::CredentialExcluded)))
        );
        // Another key excluding it creates a second token whose identify
        // probe names the first, then the new one.
        let other = blank("disk-second");
        let mut keys = Keys::one(&other);
        let second = create(
            &mut keys,
            later(),
            &[&first.credential],
            operation(),
            &mut Console::default(),
            &mut entropy(&draws),
        )
        .unwrap();
        let probe = other
            .transcript()
            .into_iter()
            .filter(|(request, _)| request[0] == 2)
            .nth(2)
            .unwrap()
            .0;
        let at = |id: &[u8]| probe.windows(id.len()).position(|window| window == id);
        assert!(at(&first.credential).unwrap() < at(&second.credential).unwrap());
        // Another device after creation: refused before the repeat.
        let mut keys = Keys::sessions(&[&other, &key], vec![vec![0], vec![1]]);
        let mut console = Console::default();
        assert_eq!(
            create(
                &mut keys,
                later(),
                &[],
                operation(),
                &mut console,
                &mut entropy(&draws),
            )
            .err(),
            Some(DiskError::OneKey)
        );
        assert_eq!(console.pins.len(), 2);
    }

    /// A two-step authorization's presented steps: each selected token and
    /// retry count asked about, the PIN typed, and whether the person ends
    /// the step.
    #[derive(Default)]
    struct Presented {
        asked: Vec<(usize, u8)>,
        pin: Option<&'static [u8]>,
        ends: bool,
    }

    fn step(token: usize, retries: u8) -> Vec<u8> {
        [b"authorize step ".as_slice(), &[token as u8, retries]].concat()
    }

    impl Steps for Presented {
        fn authorize(&mut self, token: usize, retries: u8) -> Result<(Pin, Vec<u8>), String> {
            self.asked.push((token, retries));
            if self.ends {
                return Err("the person ended it".into());
            }
            Ok((
                Pin::new(self.pin.unwrap_or(PIN).into())?,
                step(token, retries),
            ))
        }
    }

    const IDENTIFY: &[u8] = b"identify step";

    fn authorize_on(
        keys: &mut Keys,
        tokens: &[Token<'_>],
        steps: &mut Presented,
        draws: &Rc<RefCell<Vec<[u8; 32]>>>,
    ) -> Result<Released, DiskError> {
        authorize(
            keys,
            later(),
            tokens,
            &VOLUME,
            IDENTIFY,
            steps,
            &mut entropy(draws),
        )
    }

    #[test]
    fn an_authorization_binds_its_identify_step_then_the_step_it_presents() {
        let key = blank("disk-authorize");
        let created = create_on(&key, &mut Console::default()).unwrap();
        let other = blank("disk-authorize-other");
        let foreign = create_on(&other, &mut Console::default()).unwrap();
        let tokens = [token(&foreign), token(&created)];
        let draws = Rc::new(RefCell::new(Vec::new()));
        let mut keys = Keys::one(&key);
        let mut steps = Presented::default();
        let released = authorize_on(&mut keys, &tokens, &mut steps, &draws).unwrap();
        // The second token's passphrase, as its creation derived it.
        assert_eq!(released.token, 1);
        assert_eq!(released.passphrase.bytes(), created.passphrase.bytes());
        assert_eq!(steps.asked, [(1, 8)]);
        assert_eq!(keys.opened, 2);
        // Identify binds the identify step, the assertion the step the
        // caller presented for the selected token, each under authorize.
        let assertions = hashes(&key, 2, 2);
        let [identify, assertion] = assertions[assertions.len() - 2..] else {
            panic!("two assertions")
        };
        let draws = draws.borrow().clone();
        let bound = |hash: &[u8; 32], description: &[u8]| {
            draws
                .iter()
                .any(|draw| client_data_hash(Phase::Authorize, description, draw).unwrap() == *hash)
        };
        assert!(bound(&identify, IDENTIFY));
        assert!(bound(&assertion, &step(1, 8)));
        assert!(!bound(&assertion, IDENTIFY));
        assert!(!bound(&identify, &step(1, 8)));
    }

    #[test]
    fn an_authorization_refuses_a_stranger_an_ended_step_and_a_wrong_pin() {
        let key = blank("disk-authorize-refusals");
        let created = create_on(&key, &mut Console::default()).unwrap();
        let tokens = [token(&created)];
        let draws = Rc::new(RefCell::new(Vec::new()));
        // A key holding no token's credential is asked nothing.
        let stranger = blank("disk-authorize-stranger");
        let mut steps = Presented::default();
        assert_eq!(
            authorize_on(&mut Keys::one(&stranger), &tokens, &mut steps, &draws).err(),
            Some(DiskError::NotEnrolled)
        );
        assert!(steps.asked.is_empty());
        // An ended step never reaches the key's PIN check.
        let mut steps = Presented {
            ends: true,
            ..Presented::default()
        };
        assert_eq!(
            authorize_on(&mut Keys::one(&key), &tokens, &mut steps, &draws).err(),
            Some(DiskError::Ended)
        );
        assert_eq!(steps.asked, [(0, 8)]);
        // A wrong PIN is the key's typed status, and the next step names
        // the key's own falling count.
        let mut steps = Presented {
            pin: Some(b"9999"),
            ..Presented::default()
        };
        assert_eq!(
            authorize_on(&mut Keys::one(&key), &tokens, &mut steps, &draws).err(),
            Some(DiskError::Failed(Error::Status(Status::PinInvalid)))
        );
        let mut steps = Presented::default();
        assert!(authorize_on(&mut Keys::one(&key), &tokens, &mut steps, &draws).is_ok());
        assert_eq!(steps.asked, [(0, 7)]);
        // Two keys connected: refused before any session.
        let mut both = Keys::sessions(&[&key, &stranger], vec![vec![0, 1]]);
        assert_eq!(
            authorize_on(&mut both, &tokens, &mut Presented::default(), &draws).err(),
            Some(DiskError::OneKey)
        );
        assert_eq!(both.opened, 0);
    }

    /// A creation's steps as a presenting caller sees them: each phase,
    /// the credential it names and the retries, and a description built
    /// from them.
    #[derive(Default)]
    struct Stepping {
        asked: Vec<(Phase, Option<Vec<u8>>, u8)>,
        probed: Vec<Vec<u8>>,
        touches: Vec<Phase>,
        ends: Option<Phase>,
        /// A wrong PIN typed at this phase.
        wrong: Option<Phase>,
        /// Set while the probe step is presented: another key connected.
        connects: Option<Rc<Cell<bool>>>,
    }

    fn described(phase: Phase, credential: Option<&[u8]>, retries: u8) -> Vec<u8> {
        let fingerprint =
            credential.map_or([0; 4], |id| crypto::digest(id)[..4].try_into().unwrap());
        [b"step ".as_slice(), &[phase as u8, retries], &fingerprint].concat()
    }

    impl CreateSteps for Stepping {
        fn pin(&mut self, step: CreateStep<'_>, retries: u8) -> Result<(Pin, Vec<u8>), String> {
            let (phase, credential) = (step.phase(), step.credential());
            self.asked
                .push((phase, credential.map(<[u8]>::to_vec), retries));
            if self.ends == Some(phase) {
                return Err("the person ended it".into());
            }
            let pin: &[u8] = if self.wrong == Some(phase) {
                b"9999"
            } else {
                PIN
            };
            Ok((Pin::new(pin.into())?, described(phase, credential, retries)))
        }
        fn probe(&mut self, credential: &[u8]) -> Result<Vec<u8>, String> {
            self.probed.push(credential.to_vec());
            if let Some(all) = &self.connects {
                all.set(true);
            }
            if self.ends == Some(Phase::Probe) {
                return Err("the person ended it".into());
            }
            Ok(described(Phase::Probe, Some(credential), 0))
        }
        fn touch(&mut self, phase: Phase) {
            self.touches.push(phase);
        }
    }

    fn stepped_on(
        keys: &mut Keys,
        excluded: &[&[u8]],
        steps: &mut Stepping,
        draws: &Rc<RefCell<Vec<[u8; 32]>>>,
    ) -> Result<Created, DiskError> {
        create_stepped(keys, later(), excluded, &VOLUME, steps, &mut entropy(draws))
    }

    #[test]
    fn a_stepped_creation_binds_each_step_it_presents() {
        let key = blank("disk-stepped");
        let draws = Rc::new(RefCell::new(Vec::new()));
        let mut keys = Keys::one(&key);
        let mut steps = Stepping::default();
        let created = stepped_on(&mut keys, &[], &mut steps, &draws).unwrap();
        let id = created.credential.clone();
        // Create names no credential; prove and repeat name the new one,
        // as the probe does, each with the key's own retries.
        assert_eq!(
            steps.asked,
            [
                (Phase::Create, None, 8),
                (Phase::Prove, Some(id.clone()), 8),
                (Phase::Repeat, Some(id.clone()), 8),
            ]
        );
        assert_eq!(steps.probed.as_slice(), std::slice::from_ref(&id));
        assert_eq!(
            steps.touches,
            [Phase::Create, Phase::Prove, Phase::Repeat, Phase::Probe]
        );
        assert_eq!(keys.opened, 4);
        let draws = draws.borrow().clone();
        let bound = |hash: &[u8; 32], phase: Phase, description: &[u8]| {
            draws
                .iter()
                .any(|draw| client_data_hash(phase, description, draw).unwrap() == *hash)
        };
        let [create_hash] = hashes(&key, 1, 1)[..] else {
            panic!("one creation")
        };
        assert!(bound(
            &create_hash,
            Phase::Create,
            &described(Phase::Create, None, 8)
        ));
        let assertions = hashes(&key, 2, 2);
        let [prove, repeat, probe, presence] = assertions[..] else {
            panic!("four assertions")
        };
        let step = |phase, retries| described(phase, Some(&id), retries);
        assert!(bound(&prove, Phase::Prove, &step(Phase::Prove, 8)));
        assert!(bound(&repeat, Phase::Repeat, &step(Phase::Repeat, 8)));
        // Both probes bind the one probe step.
        assert!(bound(&probe, Phase::Probe, &step(Phase::Probe, 0)));
        assert!(bound(&presence, Phase::Probe, &step(Phase::Probe, 0)));
        // No step's hash binds another step's description.
        assert!(!bound(&prove, Phase::Prove, &step(Phase::Repeat, 8)));
        assert!(!bound(&repeat, Phase::Repeat, &step(Phase::Prove, 8)));
        assert!(!bound(&create_hash, Phase::Create, &step(Phase::Prove, 8)));
        assert!(!bound(&repeat, Phase::Repeat, &step(Phase::Probe, 0)));
        assert_ne!(probe, presence);
        // The passphrase is the one-description creation's: the PIN's
        // output for the salt, which a release reproduces.
        let (with_uv, _) = key.state().credentials[0].hmac.unwrap();
        let output = crypto::hmac(&with_uv, &created.salt);
        let expected = passphrase(&output, &VOLUME, &created.credential).unwrap();
        assert_eq!(created.passphrase.bytes(), expected.bytes());
        let released = release_on(
            &mut Keys::one(&key),
            &[token(&created)],
            &mut Console::default(),
        )
        .unwrap();
        assert_eq!(released.passphrase.bytes(), created.passphrase.bytes());
    }

    #[test]
    fn a_stepped_creation_ends_at_any_refused_step_and_excludes_as_create_does() {
        let draws = Rc::new(RefCell::new(Vec::new()));
        // Each PIN step and the probe step end the creation when refused,
        // before the key is asked anything more.
        let order = [Phase::Create, Phase::Prove, Phase::Repeat, Phase::Probe];
        let sessions = [1, 1, 2, 2];
        for ((at, phase), opened) in order.into_iter().enumerate().zip(sessions) {
            let key = blank("disk-stepped-ends");
            let mut keys = Keys::one(&key);
            let mut steps = Stepping {
                ends: Some(phase),
                ..Stepping::default()
            };
            assert_eq!(
                stepped_on(&mut keys, &[], &mut steps, &draws).err(),
                Some(DiskError::Ended),
                "{phase:?}"
            );
            // The refused step waits for no touch; each earlier one did.
            // No later session opens.
            assert_eq!(steps.touches, order[..at], "{phase:?}");
            assert_eq!(keys.opened, opened, "{phase:?}");
        }
        // A key connected while the probe step is presented ends the
        // creation before the identify probe's session.
        let key = blank("disk-stepped-swap");
        let other = blank("disk-stepped-other");
        let mut keys = Keys::sessions(&[&key, &other], vec![vec![0]]);
        let mut steps = Stepping {
            connects: Some(keys.all.clone()),
            ..Stepping::default()
        };
        assert_eq!(
            stepped_on(&mut keys, &[], &mut steps, &draws).err(),
            Some(DiskError::OneKey)
        );
        assert_eq!(keys.opened, 2);
        // A wrong PIN at prove is the key's typed status, and no repeat
        // step is asked; the next creation's create step names the key's
        // falling count, and each later step its own, reset by that PIN.
        let key = blank("disk-stepped-wrong");
        let mut steps = Stepping {
            wrong: Some(Phase::Prove),
            ..Stepping::default()
        };
        assert_eq!(
            stepped_on(&mut Keys::one(&key), &[], &mut steps, &draws).err(),
            Some(DiskError::Failed(Error::Status(Status::PinInvalid)))
        );
        let asked: Vec<_> = steps
            .asked
            .iter()
            .map(|(phase, _, n)| (*phase, *n))
            .collect();
        assert_eq!(asked, [(Phase::Create, 8), (Phase::Prove, 8)]);
        let mut steps = Stepping::default();
        assert!(stepped_on(&mut Keys::one(&key), &[], &mut steps, &draws).is_ok());
        let asked: Vec<_> = steps
            .asked
            .iter()
            .map(|(phase, _, n)| (*phase, *n))
            .collect();
        assert_eq!(
            asked,
            [(Phase::Create, 7), (Phase::Prove, 8), (Phase::Repeat, 8)]
        );
        // A 256-byte credential ID ends the creation before a prove step.
        let long = Virtual::new(
            Config {
                id_length: 256,
                max_id: None,
                ..Config::default()
            },
            Some(PIN),
            "disk-stepped-long",
        );
        let mut steps = Stepping::default();
        assert_eq!(
            stepped_on(&mut Keys::one(&long), &[], &mut steps, &draws).err(),
            Some(DiskError::CredentialLength)
        );
        assert_eq!(steps.asked, [(Phase::Create, None, 8)]);
        // The exclusion list is create's: a key holding an excluded
        // credential is refused at makeCredential, asked no proof.
        let key = blank("disk-stepped-excluded");
        let first = create_on(&key, &mut Console::default()).unwrap();
        let mut steps = Stepping::default();
        assert_eq!(
            stepped_on(
                &mut Keys::one(&key),
                &[&first.credential],
                &mut steps,
                &draws
            )
            .err(),
            Some(DiskError::Failed(Error::Status(Status::CredentialExcluded)))
        );
        assert_eq!(steps.asked, [(Phase::Create, None, 8)]);
    }
}
