//! Root's one bounded notebook token exchange, on an inherited capability.

use super::lifecycle::{
    AssertRequest, Computer, EnrollRequest, Hardware, Presented, Purpose, TokenError, Tokens,
};
use super::{
    host, Computers, Factor, Kind, Role, Sealed, VerificationKey, MAX_CREDENTIAL, MAX_SLOTS,
    PRIMARY_NAME,
};
use crate::consent::{Notebook, Operation, Request};
use crate::fido_device::{Device, Session, MAX_LIFETIME};
use crate::fido_pin::Pin;
use crate::fido_transaction::{Error as Transaction, PinPurpose, Status};
use crate::notebook_wire::{field, Bytes, Reader, Wire};
use std::path::Path;
use std::time::{Duration, Instant};
use td_tpm::{device_pin, AuthRefusal, Client, Transport, UnsealError};

/// The HMAC message prefix of a computer key's PIN authValue, its own
/// domain beside the disk's (td-tpm's `device_pin`).
const COMPUTER_PIN: &[u8] = b"td-secret/portable/tpm-pin/v1";

/// The worker's absolute lifetime: longer than root's 120-second step
/// deadline, so root decides a step's end and relays it.
const LIFETIME: Duration = Duration::from_secs(130);

struct Deadlines {
    worker: Instant,
    fido: Instant,
}

impl Deadlines {
    fn from_start(start: Instant) -> Result<Self, &'static str> {
        Ok(Self {
            worker: start
                .checked_add(LIFETIME)
                .ok_or("notebook deadline overflow")?,
            fido: start
                .checked_add(MAX_LIFETIME)
                .ok_or("FIDO deadline overflow")?,
        })
    }
}

/// The wrong PINs a computer step leaves to the disk: it is refused when
/// fewer than this many tries remain before the TPM's lockout, which the
/// disk's TPM PIN shares, so the notebook cannot lock the disk out alone.
const HEADROOM: u32 = 8;

/// Why a step released nothing: the token's own refusal, which root hears
/// as its code (`45`) and relays, or a fault of this worker's, which ends
/// it unsuccessfully.
#[derive(Debug)]
enum Refusal {
    Token(TokenError),
    Fault(String),
}
impl From<String> for Refusal {
    fn from(fault: String) -> Self {
        Self::Fault(fault)
    }
}

struct Exchange<'a> {
    purpose: Purpose,
    role: Role,
    request: Token<'a>,
}
enum Token<'a> {
    Enroll {
        challenge: [u8; 32],
        proof: [u8; 32],
        user: [u8; 32],
        salt: [u8; 32],
        excluded: Vec<&'a [u8]>,
    },
    Assert {
        credential: &'a [u8],
        factor: Factor,
        challenge: [u8; 32],
    },
    /// Seal a fresh secret in this computer's TPM, unless it is one of
    /// the retained computer keys' `computers`.
    Seal { computers: Computers },
    /// Unseal a computer key's sealed object.
    Unseal { credential: &'a [u8] },
}

impl<'a> Exchange<'a> {
    fn decode(bytes: &'a [u8], binding: &Notebook) -> Result<Self, String> {
        if !binding.valid() || binding.step != 0 || crate::crypto::digest(bytes) != binding.token {
            return Err("notebook token request changed its binding".into());
        }
        let mut input = Reader::new(bytes);
        let tag = input.byte()?;
        if tag != binding.method {
            return Err("notebook token method changed".into());
        }
        let purpose = match input.byte()? {
            1 => Purpose::CreatePrimary,
            2 => Purpose::CreateBackup,
            3 => Purpose::Unlock,
            5 => Purpose::AuthorizeAddKey,
            6 => Purpose::AddKey,
            7 => Purpose::AuthorizeReplaceKey,
            8 => Purpose::ReplaceKey,
            9 => Purpose::Import,
            _ => return Err("invalid notebook token purpose".into()),
        };
        let role = Role::decode(input.byte()?)?;
        if purpose as u8 != binding.purpose || role as u8 != binding.role {
            return Err("notebook token presentation changed".into());
        }
        let enrollment = matches!(
            purpose,
            Purpose::CreatePrimary | Purpose::CreateBackup | Purpose::AddKey | Purpose::ReplaceKey
        );
        let request = match tag {
            0x30 if enrollment => {
                let challenge = input.array()?;
                let proof = input.array()?;
                let user = input.array()?;
                let salt = input.array()?;
                let count = usize::from(input.byte()?);
                if count > MAX_SLOTS {
                    return Err("too many excluded notebook keys".into());
                }
                let mut excluded = Vec::with_capacity(count);
                for _ in 0..count {
                    let credential = input.field(MAX_CREDENTIAL)?;
                    if credential.is_empty() || excluded.contains(&credential) {
                        return Err("invalid excluded notebook credential".into());
                    }
                    excluded.push(credential);
                }
                Token::Enroll {
                    challenge,
                    proof,
                    user,
                    salt,
                    excluded,
                }
            }
            0x31 => {
                let credential = input.field(MAX_CREDENTIAL)?;
                if credential.is_empty() {
                    return Err("empty notebook credential".into());
                }
                let key = VerificationKey::from_cose(input.field(77)?)?;
                Token::Assert {
                    credential,
                    factor: Factor::SecurityKey {
                        key,
                        salt: input.array()?,
                    },
                    challenge: input.array()?,
                }
            }
            0x32 if enrollment => {
                let names = input.field(MAX_SLOTS * PRIMARY_NAME)?;
                if names.len() % PRIMARY_NAME != 0 {
                    return Err("invalid retained computer primaries".into());
                }
                let mut computers = Computers::NONE;
                for at in (0..names.len()).step_by(PRIMARY_NAME) {
                    let name: &[u8; PRIMARY_NAME] = names
                        .get(at..at + PRIMARY_NAME)
                        .and_then(|name| name.try_into().ok())
                        .ok_or("invalid retained computer primary")?;
                    if name.get(..2) != Some(&[0, 11]) {
                        return Err("invalid retained computer primary".into());
                    }
                    computers.hold(name)?;
                }
                Token::Seal { computers }
            }
            0x33 => {
                let credential = input.field(MAX_CREDENTIAL)?;
                Sealed::decode(credential)?;
                Token::Unseal { credential }
            }
            _ => return Err("notebook token method differs from its purpose".into()),
        };
        input.finish()?;
        Ok(Self {
            purpose,
            role,
            request,
        })
    }

    fn execute(&self, tokens: &mut impl Tokens) -> Result<Bytes, Refusal> {
        let mut out = Bytes(Vec::new());
        match &self.request {
            Token::Enroll {
                challenge,
                proof,
                user,
                salt,
                excluded,
            } => {
                let enrolled = tokens
                    .enroll(
                        Presented {
                            purpose: self.purpose,
                            role: self.role,
                            credential: None,
                        },
                        EnrollRequest {
                            kind: Kind::SecurityKey,
                            challenge: *challenge,
                            proof_challenge: *proof,
                            user: *user,
                            salt: *salt,
                            excluded,
                            computers: &Computers::NONE,
                        },
                    )
                    .map_err(Refusal::Token)?;
                let Factor::SecurityKey { key, salt } = &enrolled.factor else {
                    return Err(Refusal::Fault("a security key enrolled a computer".into()));
                };
                out.push(0x30)?;
                field(&mut out, &enrolled.credential)?;
                field(&mut out, key.cose())?;
                out.extend(salt)?;
                out.extend(&enrolled.secret.0)?;
                out.extend(&enrolled.counter.to_be_bytes())?;
            }
            Token::Assert {
                credential,
                factor,
                challenge,
            } => {
                let asserted = tokens
                    .assert(
                        Presented {
                            purpose: self.purpose,
                            role: self.role,
                            credential: Some(credential),
                        },
                        AssertRequest {
                            credential,
                            factor,
                            challenge: *challenge,
                        },
                    )
                    .map_err(Refusal::Token)?;
                out.push(0x31)?;
                out.extend(&asserted.secret.0)?;
                out.extend(&asserted.counter.to_be_bytes())?;
            }
            Token::Seal { .. } | Token::Unseal { .. } => {
                return Err(Refusal::Fault(
                    "a computer key is not a security key".into(),
                ))
            }
        }
        Ok(out)
    }
}

fn computer(refusal: Computer) -> Refusal {
    Refusal::Token(TokenError::Computer(refusal))
}

/// The proved storage primary and a TPM client admitted by `begin`, all
/// before any PIN is asked for.
fn tpm(
    primary: &Path,
    held: &Computers,
) -> Result<([u8; device_pin::PRIMARY_NAME_LEN], Client<td_tpm::Device>), Refusal> {
    let name = device_pin::admitted_primary(primary).map_err(|_| computer(Computer::Unadmitted))?;
    let device = td_tpm::Device::open().map_err(|_| computer(Computer::Unreachable))?;
    let client = begin(&name, Client::new(device), held)?;
    Ok((name, client))
}

/// Admits the TPM behind `client` for a computer step under `name`: not
/// one of the `held` retained computers, td's dictionary-attack policy,
/// and `HEADROOM` tries left before its lockout.
fn begin<T: Transport>(
    name: &[u8; PRIMARY_NAME],
    mut client: Client<T>,
    held: &Computers,
) -> Result<Client<T>, Refusal> {
    if held.contains(name) {
        return Err(computer(Computer::Held));
    }
    let attack = client
        .dictionary_attack()
        .map_err(|_| computer(Computer::Unreachable))?;
    if attack.in_lockout || (attack.lockout_auth_set && attack.counter >= attack.max_tries) {
        return Err(computer(Computer::Lockout));
    }
    // A TPM that allows no more than the headroom could never leave it:
    // that is td's policy unmet, not a counter run down.
    if !attack.holds_td_policy() || attack.max_tries <= HEADROOM {
        return Err(computer(Computer::Policy));
    }
    if attack.counter.saturating_add(HEADROOM) >= attack.max_tries {
        return Err(computer(Computer::Headroom));
    }
    Ok(client)
}

/// What a TPM operation that ended without a PIN refusal tells: td-tpm
/// found the TPM's storage primary not the one this boot proved, or the
/// TPM sealed under another policy than asked; anything else is a TPM
/// that could not be reached or answered otherwise.
fn tpm_failure(message: &str) -> Refusal {
    if message == td_tpm::OTHER_PRIMARY || message == td_tpm::CHANGED_POLICY {
        computer(Computer::Changed)
    } else {
        computer(Computer::Unreachable)
    }
}

/// An unseal's failure: a wrong PIN reads as a security key's, which the
/// TPM counted; a lockout as the TPM's.
fn unseal_failure(error: &UnsealError) -> Refusal {
    match error.authorization() {
        Some(AuthRefusal::AuthFail) => {
            Refusal::Token(TokenError::Failed(Transaction::Status(Status::PinInvalid)))
        }
        Some(AuthRefusal::Lockout) => computer(Computer::Lockout),
        None => tpm_failure(&error.message),
    }
}

/// Whether a sealed object carries td's device PIN policy, counted by the
/// TPM's lockout; any other is not one td sealed.
fn td_object(sealed: &Sealed<'_>) -> Result<(), Refusal> {
    match td_tpm::validate_auth_sealed_public(sealed.public, &td_tpm::device_pin_policy_digest()) {
        Ok(td_tpm::DA_SEALED_ATTRIBUTES) => Ok(()),
        _ => Err(computer(Computer::Object)),
    }
}

/// A computer key's PIN as the window gave it, in the shared device PIN
/// profile; outside it the step is refused, its bytes cleared.
fn computer_pin(
    wire: &mut Wire,
    request: &mut Request,
    purpose: PinPurpose,
) -> Result<device_pin::Pin, Refusal> {
    let bytes = pin_bytes(wire, request, purpose)?;
    device_pin::Pin::parse(&bytes.0).map_err(|_| computer(Computer::PinLength))
}

/// Seals a fresh secret under a new PIN: `32`, the sealed object, then
/// the secret. Sealing counts no guess; the repeat unseal proves the PIN.
fn seal(
    wire: &mut Wire,
    request: &mut Request,
    primary: &Path,
    held: &Computers,
    entropy: &mut host::Entropy,
) -> Result<Reply, Refusal> {
    let (name, client) = tpm(primary, held)?;
    let pin = computer_pin(wire, request, PinPurpose::Creation)?;
    let mut salt = [0; 32];
    entropy.fill(&mut salt)?;
    let mut secret = Secret([0; SECRET]);
    entropy.fill(&mut secret.0)?;
    // td-tpm zeroes the payload on every path.
    let mut payload = secret.0;
    let auth = pin.auth_value(COMPUTER_PIN, &salt);
    let sealed = client
        .seal_device_pin(&name, auth.expose(), &mut payload)
        .map_err(|message| tpm_failure(&message))?;
    let credential = Sealed {
        primary: &name,
        salt: &salt,
        public: &sealed.object.public,
        private: &sealed.object.private,
    };
    let size = credential.encoded_len()?;
    let mut reply = Reply::new(1 + 4 + size + SECRET)?;
    reply.put(&[0x32])?;
    reply.put(
        &u32::try_from(size)
            .map_err(|_| Refusal::Fault("oversized computer key".into()))?
            .to_be_bytes(),
    )?;
    credential.write(&mut |part| reply.put(part))?;
    reply.put(&secret.0)?;
    Ok(reply)
}

/// Unseals a computer key with its PIN: `33` and the secret. A wrong PIN
/// counts against the TPM's lockout and reads as a security key's.
fn unseal(
    wire: &mut Wire,
    request: &mut Request,
    primary: &Path,
    credential: &[u8],
) -> Result<Reply, Refusal> {
    let sealed = Sealed::decode(credential)?;
    td_object(&sealed)?;
    let (name, client) = tpm(primary, &Computers::NONE)?;
    if sealed.primary != &name {
        return Err(computer(Computer::Foreign));
    }
    let pin = computer_pin(wire, request, PinPurpose::Assertion)?;
    let auth = pin.auth_value(COMPUTER_PIN, sealed.salt);
    let secret = Bytes(
        client
            .unseal_device_pin(&name, auth.expose(), sealed.public, sealed.private)
            .map_err(|error| unseal_failure(&error))?,
    );
    if secret.0.len() != SECRET {
        return Err(Refusal::Fault(
            "computer key secret has another length".into(),
        ));
    }
    let mut reply = Reply::new(1 + SECRET)?;
    reply.put(&[0x33])?;
    reply.put(&secret.0)?;
    Ok(reply)
}

/// A computer key's secret, a vault key share.
const SECRET: usize = 32;
/// The largest computer reply: `44`, its result's count, and a sealing's
/// method, credential field and secret.
const REPLY: usize = 1 + 4 + 1 + 4 + MAX_CREDENTIAL + SECRET;

/// A secret drawn for sealing, on the stack and cleared on drop.
struct Secret([u8; SECRET]);
impl Drop for Secret {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&mut self.0);
    }
}

/// A computer step's whole reply to root, `44` and its result's field,
/// built on the stack and cleared on drop: it holds the secret, and no
/// heap buffer it passes through can leave a copy behind.
struct Reply {
    bytes: [u8; REPLY],
    len: usize,
    end: usize,
}
impl Reply {
    /// A reply whose result is `result` bytes, its head written.
    fn new(result: usize) -> Result<Self, String> {
        let count = u32::try_from(result).map_err(|_| "computer reply overflow")?;
        let mut reply = Self {
            bytes: [0; REPLY],
            len: 0,
            end: result.checked_add(5).ok_or("computer reply overflow")?,
        };
        if reply.end > REPLY {
            return Err("computer reply exceeds its bound".into());
        }
        reply.put(&[0x44])?;
        reply.put(&count.to_be_bytes())?;
        Ok(reply)
    }
    fn put(&mut self, part: &[u8]) -> Result<(), String> {
        let end = self
            .len
            .checked_add(part.len())
            .filter(|end| *end <= self.end)
            .ok_or("computer reply exceeds its count")?;
        self.bytes
            .get_mut(self.len..end)
            .ok_or("computer reply exceeds its bound")?
            .copy_from_slice(part);
        self.len = end;
        Ok(())
    }
    /// The reply, once exactly its count was written.
    fn complete(&self) -> Result<&[u8], String> {
        if self.len != self.end {
            return Err("computer reply is short of its count".into());
        }
        self.bytes
            .get(..self.len)
            .ok_or_else(|| "computer reply exceeds its bound".into())
    }
}
impl Drop for Reply {
    fn drop(&mut self) {
        self.bytes.fill(0);
        std::hint::black_box(&mut self.bytes);
    }
}

fn presentation(wire: &mut Wire, request: &Request) -> Result<(), String> {
    let encoded = request.encode();
    let mut out = Bytes(vec![0x40]);
    field(&mut out, &encoded)?;
    wire.send(&out.0)?;
    let reply = wire.receive(LIFETIME)?;
    let mut input = Reader::new(&reply.0);
    if input.byte()? != 0x41 || input.field(256)? != encoded {
        return Err("notebook presentation was not acknowledged".into());
    }
    input.finish()
}

fn prompt(wire: &mut Wire, request: &mut Request, purpose: PinPurpose) -> Result<Pin, String> {
    let mut bytes = pin_bytes(wire, request, purpose)?;
    if bytes.0.len() < 4 {
        return Err("notebook PIN is outside its profile".into());
    }
    // Moved, not copied: the field's exact copy becomes the PIN's owner.
    Pin::new(std::mem::take(&mut bytes.0).into_boxed_slice())
        .map_err(|_| "invalid notebook PIN".into())
}

/// Presents the PIN step `purpose` names and returns the printable PIN
/// root sent for it, in a clearing owner.
fn pin_bytes(wire: &mut Wire, request: &mut Request, purpose: PinPurpose) -> Result<Bytes, String> {
    let Operation::Notebook(previous) = request.operation() else {
        return Err("not a notebook presentation".into());
    };
    let mut next = previous.clone();
    next.step = match purpose {
        PinPurpose::Assertion => 1,
        PinPurpose::Creation => 2,
        PinPurpose::EnrollmentProof => 3,
    };
    if !next.follows(previous) {
        return Err("invalid notebook PIN step order".into());
    }
    *request = Request::new(*request.nonce(), request.owner(), Operation::Notebook(next))?;
    presentation(wire, request)?;
    let reply = wire.receive(LIFETIME)?;
    let mut input = Reader::new(&reply.0);
    if input.byte()? != 0x42 || input.field(256)? != request.encode() {
        return Err("notebook PIN names another presentation".into());
    }
    let bytes = Bytes(input.field(63)?.to_vec());
    input.finish()?;
    if bytes.0.is_empty() || !bytes.0.iter().all(|b| (0x20..=0x7e).contains(b)) {
        return Err("notebook PIN is outside its profile".into());
    }
    Ok(bytes)
}

pub(crate) fn run(owner: u32) -> Result<(), String> {
    let endpoint = crate::operation::startup()?;
    let deadlines = Deadlines::from_start(Instant::now())?;
    let protected = host::protect_service()?;
    let mut wire = Wire::new(endpoint)?;
    wire.bound(deadlines.worker);
    let initial = wire.receive(Duration::from_secs(5))?;
    let mut input = Reader::new(&initial.0);
    let mut request = Request::decode(input.field(256)?)?;
    if request.owner() != owner {
        return Err("notebook token owner changed".into());
    }
    let Operation::Notebook(binding) = request.operation() else {
        return Err("notebook token needs a notebook presentation".into());
    };
    let exchange = Exchange::decode(input.field(16384)?, binding)?;
    input.finish()?;
    presentation(&mut wire, &request)?;
    protected.recheck()?;
    let primary = Path::new(device_pin::ADMITTED_PRIMARY);
    let computer = match &exchange.request {
        Token::Seal { computers } => {
            let mut entropy = host::Entropy::open()?;
            Some(seal(
                &mut wire,
                &mut request,
                primary,
                computers,
                &mut entropy,
            ))
        }
        Token::Unseal { credential } => Some(unseal(&mut wire, &mut request, primary, credential)),
        Token::Enroll { .. } | Token::Assert { .. } => None,
    };
    if let Some(outcome) = computer {
        let result = match outcome {
            Ok(result) => result,
            Err(Refusal::Token(error)) => return refuse(&mut wire, &error),
            Err(Refusal::Fault(fault)) => return Err(fault),
        };
        protected.recheck()?;
        return wire.send(result.complete()?);
    }
    let devices = Device::discover()?;
    let device = match devices.as_slice() {
        [device] => *device,
        [] => return refuse(&mut wire, &TokenError::Unavailable),
        _ => return refuse(&mut wire, &TokenError::Several),
    };
    let mut entropy = host::Entropy::open()?;
    let mut tokens = Hardware {
        open: |_: Presented<'_>| {
            Session::open(device, deadlines.fido).map_err(|_| TokenError::Unavailable)
        },
        prompt: |_: Presented<'_>, purpose| prompt(&mut wire, &mut request, purpose),
        entropy: |bytes: &mut [u8]| entropy.fill(bytes),
    };
    let result = match exchange.execute(&mut tokens) {
        Ok(result) => result,
        Err(Refusal::Token(error)) => {
            return refuse(&mut wire, &error);
        }
        Err(Refusal::Fault(fault)) => return Err(fault),
    };
    protected.recheck()?;
    let mut out = Bytes(vec![0x44]);
    field(&mut out, &result.0)?;
    wire.send(&out.0)
}

/// Tells root why the step released nothing, so the window can say so:
/// `45` and the refusal's code, which carries no PIN or token output.
fn refuse(wire: &mut Wire, error: &TokenError) -> Result<(), String> {
    wire.send(&[0x45, super::service_tokens::refusal(error)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn immediate_approval_fits_the_fido_session_lifetime() {
        let start = Instant::now();
        let deadlines = Deadlines::from_start(start).unwrap();
        assert_eq!(deadlines.fido - start, crate::fido_device::MAX_LIFETIME);
        assert!(deadlines.worker > deadlines.fido);
    }

    fn enrollment() -> (Bytes, Notebook) {
        let mut bytes = Bytes(vec![0x30, 1, 1]);
        for value in 1..=4 {
            bytes.extend(&[value; 32]).unwrap();
        }
        bytes.push(0).unwrap();
        let binding = Notebook {
            method: 0x30,
            action: 1,
            purpose: 1,
            role: 1,
            step: 0,
            operation: [9; 32],
            token: crate::crypto::digest(&bytes.0),
        };
        (bytes, binding)
    }

    #[test]
    fn request_must_match_the_approved_method_purpose_role_and_digest() {
        let (mut bytes, mut binding) = enrollment();
        assert!(Exchange::decode(&bytes.0, &binding).is_ok());
        binding.action = 3;
        assert!(Exchange::decode(&bytes.0, &binding).is_err());
        binding.action = 1;
        binding.role = 2;
        assert!(Exchange::decode(&bytes.0, &binding).is_err());
        binding.role = 1;
        bytes.0[0] = 0x31;
        binding.token = crate::crypto::digest(&bytes.0);
        assert!(Exchange::decode(&bytes.0, &binding).is_err());
        bytes.0[0] = 0x30;
        binding.token = crate::crypto::digest(&bytes.0);
        bytes.push(0).unwrap();
        assert!(Exchange::decode(&bytes.0, &binding).is_err());
        binding.token = crate::crypto::digest(&bytes.0);
        assert!(Exchange::decode(&bytes.0, &binding).is_err());
        for n in 0..bytes.0.len() - 1 {
            binding.token = crate::crypto::digest(&bytes.0[..n]);
            assert!(Exchange::decode(&bytes.0[..n], &binding).is_err());
        }
    }

    fn computer_binding(method: u8, action: u8, purpose: u8, role: u8, bytes: &Bytes) -> Notebook {
        Notebook {
            method,
            action,
            purpose,
            role,
            step: 0,
            operation: [9; 32],
            token: crate::crypto::digest(&bytes.0),
        }
    }

    /// A computer reply is `44` and its result's field, exactly its
    /// count: neither more nor less is sent, and the largest sealing fits.
    #[test]
    fn a_computer_reply_is_exactly_its_counted_result() {
        let mut reply = Reply::new(1 + SECRET).unwrap();
        assert!(reply.complete().is_err());
        reply.put(&[0x33]).unwrap();
        reply.put(&[0x77; SECRET]).unwrap();
        let mut expected = vec![0x44, 0, 0, 0, 33, 0x33];
        expected.extend_from_slice(&[0x77; SECRET]);
        assert_eq!(reply.complete().unwrap(), expected);
        assert!(reply.put(&[0]).is_err());
        let largest = 1 + 4 + MAX_CREDENTIAL + SECRET;
        let mut sealing = Reply::new(largest).unwrap();
        sealing.put(&vec![1; largest]).unwrap();
        assert_eq!(sealing.complete().unwrap().len(), 5 + largest);
        assert!(Reply::new(largest + 1).is_err());
    }

    #[test]
    fn computer_steps_seal_only_for_an_enrollment_and_unseal_a_sealed_object() {
        let seal = Bytes(vec![0x32, 6, 2, 0, 0, 0, 0]);
        let binding = computer_binding(0x32, 5, 6, 2, &seal);
        assert!(matches!(
            Exchange::decode(&seal.0, &binding).unwrap().request,
            Token::Seal { computers } if computers.names().is_empty()
        ));
        // Sealing is enrollment only, and carries only the retained
        // computers' primary Names, each a SHA-256 Name named once.
        let unlock = Bytes(vec![0x32, 3, 1, 0, 0, 0, 0]);
        assert!(Exchange::decode(&unlock.0, &computer_binding(0x32, 3, 3, 1, &unlock)).is_err());
        let trailing = Bytes(vec![0x32, 6, 2, 0, 0, 0, 0, 0]);
        assert!(
            Exchange::decode(&trailing.0, &computer_binding(0x32, 5, 6, 2, &trailing)).is_err()
        );
        let mut name = [4; PRIMARY_NAME];
        name[..2].copy_from_slice(&[0, 11]);
        let names = |names: &[u8]| {
            let mut out = Bytes(vec![0x32, 6, 2]);
            field(&mut out, names).unwrap();
            out
        };
        let held = names(&name);
        assert!(matches!(
            Exchange::decode(&held.0, &computer_binding(0x32, 5, 6, 2, &held)).unwrap().request,
            Token::Seal { computers } if computers.names() == [name]
        ));
        let twice = names(&[name, name].concat());
        assert!(Exchange::decode(&twice.0, &computer_binding(0x32, 5, 6, 2, &twice)).is_err());
        let sha1 = names(&[4; PRIMARY_NAME]);
        assert!(Exchange::decode(&sha1.0, &computer_binding(0x32, 5, 6, 2, &sha1)).is_err());
        let partial = names(&name[..33]);
        assert!(Exchange::decode(&partial.0, &computer_binding(0x32, 5, 6, 2, &partial)).is_err());
        let mut nine = Vec::new();
        for machine in 0..9u8 {
            let mut other = name;
            other[2] = machine;
            nine.extend_from_slice(&other);
        }
        let nine = names(&nine);
        assert!(Exchange::decode(&nine.0, &computer_binding(0x32, 5, 6, 2, &nine)).is_err());
        let credential = crate::portable::tests::computer_credential(4, 4);
        let mut unseal = Bytes(vec![0x33, 3, 1]);
        field(&mut unseal, &credential).unwrap();
        let exchange =
            Exchange::decode(&unseal.0, &computer_binding(0x33, 3, 3, 1, &unseal)).unwrap();
        assert!(matches!(exchange.request, Token::Unseal { credential: c } if c == credential));
        // A method other than the approved one, or no sealed object.
        assert!(Exchange::decode(&unseal.0, &computer_binding(0x31, 3, 3, 1, &unseal)).is_err());
        let mut bogus = Bytes(vec![0x33, 3, 1]);
        field(&mut bogus, b"token-0-credential-0").unwrap();
        assert!(Exchange::decode(&bogus.0, &computer_binding(0x33, 3, 3, 1, &bogus)).is_err());
    }

    #[test]
    fn no_proved_primary_refuses_before_any_pin_is_asked_for() {
        let (left, right) = UnixStream::pair().unwrap();
        let mut wire = Wire::new(left).unwrap();
        let seal = Bytes(vec![0x32, 6, 2, 0, 0, 0, 0]);
        let mut request = Request::new(
            [3; 32],
            1000,
            Operation::Notebook(computer_binding(0x32, 5, 6, 2, &seal)),
        )
        .unwrap();
        let mut entropy = host::Entropy::open().unwrap();
        let missing = Path::new("/nonexistent/td-passkey-primary");
        assert!(matches!(
            super::seal(
                &mut wire,
                &mut request,
                missing,
                &Computers::NONE,
                &mut entropy
            ),
            Err(Refusal::Token(TokenError::Computer(Computer::Unadmitted)))
        ));
        let credential = crate::portable::tests::computer_credential(4, 4);
        assert!(matches!(
            super::unseal(&mut wire, &mut request, missing, &credential),
            Err(Refusal::Token(TokenError::Computer(Computer::Unadmitted)))
        ));
        // A sealed object under another policy is refused before the TPM.
        let mut owned = crate::portable::tests::device_pin_public(4, td_tpm::NO_DA);
        let mut primary = [4; PRIMARY_NAME];
        primary[..2].copy_from_slice(&[0, 11]);
        let other = crate::portable::tests::sealed_bytes(&Sealed {
            primary: &primary,
            salt: &[4; 32],
            public: &owned,
            private: &[1; 90],
        })
        .unwrap();
        assert!(matches!(
            super::unseal(&mut wire, &mut request, missing, &other),
            Err(Refusal::Token(TokenError::Computer(Computer::Object)))
        ));
        owned.fill(0);
        drop(wire);
        let mut peer = Wire::new(right).unwrap();
        assert!(peer.receive(Duration::from_millis(50)).is_err());
    }

    /// A TPM that answers only the two property reads a computer step's
    /// admission makes, with `attack`'s values.
    struct Properties(td_tpm::DictionaryAttack, u32);
    impl Transport for Properties {
        fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
            self.1 += 1;
            let first = u32::from_be_bytes(command[command.len() - 8..][..4].try_into().unwrap());
            let values: Vec<(u32, u32)> = if first == td_tpm::PT_PERMANENT {
                let permanent =
                    u32::from(self.0.lockout_auth_set) << 2 | u32::from(self.0.in_lockout) << 9;
                vec![(first, permanent)]
            } else {
                let attack = self.0;
                [
                    attack.counter,
                    attack.max_tries,
                    attack.interval,
                    attack.recovery,
                ]
                .into_iter()
                .enumerate()
                .map(|(at, value)| (first + at as u32, value))
                .collect()
            };
            let mut reply = vec![0x80, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            reply.extend_from_slice(&6u32.to_be_bytes());
            reply.extend_from_slice(&(values.len() as u32).to_be_bytes());
            for (property, value) in values {
                reply.extend_from_slice(&property.to_be_bytes());
                reply.extend_from_slice(&value.to_be_bytes());
            }
            let size = (reply.len() as u32).to_be_bytes();
            reply[2..6].copy_from_slice(&size);
            Ok(reply)
        }
    }

    #[test]
    fn a_computer_step_is_admitted_only_with_td_s_policy_and_headroom_left() {
        let mut name = [4; PRIMARY_NAME];
        name[..2].copy_from_slice(&[0, 11]);
        let attack = |f: fn(&mut td_tpm::DictionaryAttack)| {
            let mut attack = td_tpm::DictionaryAttack {
                lockout_auth_set: true,
                in_lockout: false,
                counter: 0,
                max_tries: 32,
                interval: 600,
                recovery: 86400,
            };
            f(&mut attack);
            Client::new(Properties(attack, 0))
        };
        let refusal = |client, held: &Computers| match begin(&name, client, held) {
            Ok(_) => None,
            Err(Refusal::Token(TokenError::Computer(why))) => Some(why),
            Err(other) => panic!("{other:?}"),
        };
        assert_eq!(refusal(attack(|_| ()), &Computers::NONE), None);
        // Up to the 24th wrong PIN leaves eight tries to the disk.
        assert_eq!(refusal(attack(|a| a.counter = 23), &Computers::NONE), None);
        assert_eq!(
            refusal(attack(|a| a.counter = 24), &Computers::NONE),
            Some(Computer::Headroom)
        );
        // A TPM allowing eight tries or fewer can never leave the
        // headroom: td's policy is unmet, whatever its counter. Nine
        // allow one notebook try from zero.
        for max in [1, 8] {
            assert_eq!(
                refusal(
                    attack(match max {
                        1 => |a: &mut td_tpm::DictionaryAttack| a.max_tries = 1,
                        _ => |a: &mut td_tpm::DictionaryAttack| a.max_tries = 8,
                    }),
                    &Computers::NONE
                ),
                Some(Computer::Policy)
            );
        }
        assert_eq!(refusal(attack(|a| a.max_tries = 9), &Computers::NONE), None);
        assert_eq!(
            refusal(
                attack(|a| {
                    a.max_tries = 9;
                    a.counter = 1;
                }),
                &Computers::NONE
            ),
            Some(Computer::Headroom)
        );
        assert_eq!(
            refusal(attack(|a| a.counter = 32), &Computers::NONE),
            Some(Computer::Lockout)
        );
        assert_eq!(
            refusal(attack(|a| a.in_lockout = true), &Computers::NONE),
            Some(Computer::Lockout)
        );
        assert_eq!(
            refusal(attack(|a| a.lockout_auth_set = false), &Computers::NONE),
            Some(Computer::Policy)
        );
        assert_eq!(
            refusal(attack(|a| a.interval = 1), &Computers::NONE),
            Some(Computer::Policy)
        );
        // A computer the vault already holds is refused before the TPM.
        let mut held = Computers::NONE;
        held.hold(&name).unwrap();
        let mut client = Client::new(Properties(
            td_tpm::DictionaryAttack {
                lockout_auth_set: true,
                in_lockout: false,
                counter: 0,
                max_tries: 32,
                interval: 600,
                recovery: 86400,
            },
            0,
        ));
        assert!(matches!(
            begin(&name, client, &held),
            Err(Refusal::Token(TokenError::Computer(Computer::Held)))
        ));
        client = Client::new(Properties(
            td_tpm::DictionaryAttack {
                lockout_auth_set: true,
                in_lockout: false,
                counter: 0,
                max_tries: 32,
                interval: 600,
                recovery: 86400,
            },
            0,
        ));
        let mut other = name;
        other[2] = 5;
        assert!(begin(&other, client, &held).is_ok());
    }

    #[test]
    fn tpm_failures_name_their_cause() {
        let failed = |rc: Option<u32>, message: &str| UnsealError {
            refusal: rc.map(|rc| td_tpm::Refusal { command: 0x15e, rc }),
            message: message.into(),
        };
        let cause = |refusal| match refusal {
            Refusal::Token(error) => error,
            Refusal::Fault(fault) => panic!("{fault}"),
        };
        assert_eq!(
            cause(unseal_failure(&failed(Some(td_tpm::RC_AUTH_FAIL), "x"))),
            TokenError::Failed(Transaction::Status(Status::PinInvalid))
        );
        assert_eq!(
            cause(unseal_failure(&failed(Some(td_tpm::RC_LOCKOUT), "x"))),
            TokenError::Computer(Computer::Lockout)
        );
        assert_eq!(
            cause(unseal_failure(&failed(None, td_tpm::OTHER_PRIMARY))),
            TokenError::Computer(Computer::Changed)
        );
        assert_eq!(
            cause(tpm_failure(td_tpm::CHANGED_POLICY)),
            TokenError::Computer(Computer::Changed)
        );
        assert_eq!(
            cause(unseal_failure(&failed(Some(0x101), "TPM command refused"))),
            TokenError::Computer(Computer::Unreachable)
        );
        assert_eq!(
            cause(tpm_failure("open /dev/tpmrm0")),
            TokenError::Computer(Computer::Unreachable)
        );
    }

    #[test]
    fn the_service_seals_and_unseals_a_computer_key_through_root() {
        use super::super::lifecycle::{self, tests::Random, Directory};
        use super::super::service_tokens::{Remote, DECLINED};
        use std::os::unix::fs::MetadataExt;
        let temp = crate::test_temp::TempDir::new("notebook-token-computer").unwrap();
        let file = std::fs::File::open(temp.path()).unwrap();
        let directory = Directory {
            owner: file.metadata().unwrap().uid(),
            file,
        };
        let credential = crate::portable::tests::computer_credential(4, 4);
        let (left, right) = UnixStream::pair().unwrap();
        let sealed = credential.clone();
        let root = std::thread::spawn(move || -> Result<Vec<Vec<u8>>, String> {
            let mut wire = Wire::new(right)?;
            let mut seen = Vec::new();
            for answer in 0..4 {
                let bytes = wire.receive(LIFETIME)?;
                seen.push(bytes.0.clone());
                let reply = match (answer, bytes.0[0]) {
                    (0, 0x32) => {
                        let mut out = Bytes(vec![0x32]);
                        field(&mut out, &sealed)?;
                        out.extend(&[0x77; 32])?;
                        out
                    }
                    (1 | 2, 0x33) => {
                        let mut out = Bytes(vec![0x33]);
                        out.extend(&[0x77; 32])?;
                        out
                    }
                    (3, 0x33) => Bytes(vec![DECLINED, 0x25]),
                    _ => return Err("unexpected computer request".into()),
                };
                wire.send(&reply.0)?;
            }
            Ok(seen)
        });
        let mut wire = Wire::new(left).unwrap();
        let mut remote = Remote::new(&mut wire);
        let session = lifecycle::create(
            &directory,
            &mut remote,
            &mut Random(0),
            Kind::Computer,
            None,
        )
        .unwrap();
        drop(session);
        lifecycle::unlock(&directory, &credential, &mut remote, &mut Random(1)).unwrap();
        assert_eq!(
            lifecycle::unlock(&directory, &credential, &mut remote, &mut Random(2)).err(),
            Some(lifecycle::Error::Token(TokenError::Failed(
                Transaction::Status(Status::PinInvalid)
            )))
        );
        assert!(!remote.revoked());
        let seen = root.join().unwrap().unwrap();
        assert_eq!(seen[0], [0x32, 1, 1, 0, 0, 0, 0]);
        assert_eq!(seen[1][..3], [0x33, 1, 1]);
        assert_eq!(seen[2][..3], [0x33, 3, 1]);
        assert!(seen[2].ends_with(&credential));
    }

    #[test]
    fn full_lifecycle_creation_uses_enrollment_and_repeat_for_both_keys() {
        use super::super::lifecycle::{
            self,
            tests::{Bench, Random},
            Directory,
        };
        use std::os::unix::fs::MetadataExt;
        let temp = crate::test_temp::TempDir::new("notebook-token-lifecycle").unwrap();
        let file = std::fs::File::open(temp.path()).unwrap();
        let directory = Directory {
            owner: file.metadata().unwrap().uid(),
            file,
        };
        let (left, right) = UnixStream::pair().unwrap();
        let root = std::thread::spawn(move || -> Result<Vec<u8>, String> {
            let mut wire = Wire::new(right)?;
            let mut bench = Bench::new(2);
            bench.present(&[0, 0, 1, 1]);
            let mut methods = Vec::new();
            for _ in 0..4 {
                let bytes = wire.receive(LIFETIME)?;
                let binding = Notebook {
                    method: bytes.0[0],
                    action: 2,
                    purpose: bytes.0[1],
                    role: bytes.0[2],
                    step: 0,
                    operation: [9; 32],
                    token: crate::crypto::digest(&bytes.0),
                };
                let exchange = Exchange::decode(&bytes.0, &binding)?;
                let result = exchange
                    .execute(&mut bench)
                    .map_err(|refusal| format!("{refusal:?}"))?;
                methods.push(bytes.0[0]);
                wire.send(&result.0)?;
            }
            Ok(methods)
        });
        let mut wire = Wire::new(left).unwrap();
        let mut remote = super::super::service_tokens::Remote::new(&mut wire);
        let result = lifecycle::create(
            &directory,
            &mut remote,
            &mut Random(0),
            Kind::SecurityKey,
            Some(Kind::SecurityKey),
        );
        let handled = root.join().unwrap();
        assert!(result.is_ok(), "{handled:?}");
        assert_eq!(handled.unwrap(), [0x30, 0x31, 0x30, 0x31]);
        assert_eq!(lifecycle::keys(&directory).unwrap().len(), 2);
    }

    /// Root relays a refusal as its code: each token refusal comes back
    /// as itself (a protocol reason's text excepted), an unknown code is
    /// the host's, and a declined step fails creation with its reason
    /// while the service's token authority stays good.
    #[test]
    fn refusals_cross_root_as_codes_and_a_decline_revokes_nothing() {
        use super::super::lifecycle::{self, tests::Random, Directory};
        use super::super::service_tokens::{refusal, refused, Remote, DECLINED};
        use crate::fido_transaction::{Error as Transaction, Status};
        use std::os::unix::fs::MetadataExt;
        let mut named = 0;
        for code in 0..=u8::MAX {
            let error = refused(code);
            if refusal(&error) == code {
                named += 1;
            } else {
                assert_eq!(refusal(&error), 0x03, "{code:#x}");
            }
        }
        // Four device and host kinds, seven transaction kinds, fourteen
        // statuses, eleven computer refusals; root's superseded code reads
        // as the host's.
        assert_eq!(named, 4 + 7 + 14 + 11);
        assert_eq!(
            super::super::pass::token_text(&refused(0x34)),
            "this computer's TPM refuses PINs for now after too many wrong ones; try again later"
        );
        assert_eq!(
            refused(0x25),
            TokenError::Failed(Transaction::Status(Status::PinInvalid))
        );
        assert_eq!(super::super::pass::token_text(&refused(0x25)), "wrong PIN");
        assert_eq!(
            refusal(&refused(super::super::service_tokens::SUPERSEDED)),
            0x03
        );

        let temp = crate::test_temp::TempDir::new("notebook-token-decline").unwrap();
        let file = std::fs::File::open(temp.path()).unwrap();
        let directory = Directory {
            owner: file.metadata().unwrap().uid(),
            file,
        };
        let (left, right) = UnixStream::pair().unwrap();
        let root = std::thread::spawn(move || {
            let mut wire = Wire::new(right).unwrap();
            let request = wire.receive(LIFETIME).unwrap();
            assert_eq!(request.0[0], 0x30);
            wire.send(&[DECLINED, 0x25]).unwrap();
        });
        let mut wire = Wire::new(left).unwrap();
        let mut remote = Remote::new(&mut wire);
        let result = lifecycle::create(
            &directory,
            &mut remote,
            &mut Random(0),
            Kind::SecurityKey,
            None,
        );
        root.join().unwrap();
        assert!(matches!(
            result,
            Err(lifecycle::Error::Token(TokenError::Failed(
                Transaction::Status(Status::PinInvalid)
            )))
        ));
        assert!(!remote.revoked());
        assert!(
            lifecycle::keys(&directory).is_err() || lifecycle::keys(&directory).unwrap().is_empty()
        );
    }

    #[test]
    fn proved_results_encode_for_the_service_and_refusal_releases_nothing() {
        use super::super::lifecycle::tests::Bench;
        let (bytes, mut binding) = enrollment();
        let mut bench = Bench::new(1);
        bench.present(&[0, 0]);
        let enrolled = Exchange::decode(&bytes.0, &binding)
            .unwrap()
            .execute(&mut bench)
            .unwrap();
        let mut result = Reader::new(&enrolled.0);
        assert_eq!(result.byte().unwrap(), 0x30);
        let credential = result.field(MAX_CREDENTIAL).unwrap();
        let key = result.field(77).unwrap();
        let salt: [u8; 32] = result.array().unwrap();
        let secret: [u8; 32] = result.array().unwrap();
        let _: [u8; 4] = result.array().unwrap();
        result.finish().unwrap();
        let mut assertion = Bytes(vec![0x31, 3, 1]);
        field(&mut assertion, credential).unwrap();
        field(&mut assertion, key).unwrap();
        assertion.extend(&salt).unwrap();
        assertion.extend(&[8; 32]).unwrap();
        binding.action = 3;
        binding.method = 0x31;
        binding.purpose = 3;
        binding.token = crate::crypto::digest(&assertion.0);
        let exchange = Exchange::decode(&assertion.0, &binding).unwrap();
        let asserted = exchange.execute(&mut bench).unwrap();
        let mut result = Reader::new(&asserted.0);
        assert_eq!(result.byte().unwrap(), 0x31);
        assert_eq!(result.array::<32>().unwrap(), secret);
        let _: [u8; 4] = result.array().unwrap();
        result.finish().unwrap();
        bench.present(&[0]);
        bench.fail = Some((2, super::super::lifecycle::TokenError::Unavailable));
        assert!(exchange.execute(&mut bench).is_err());
    }

    #[test]
    fn pin_is_bound_to_the_exact_acknowledged_step_and_cannot_skip_proof() {
        let (_, binding) = enrollment();
        let mut request = Request::new([7; 32], 1000, Operation::Notebook(binding)).unwrap();
        let (left, right) = UnixStream::pair().unwrap();
        let mut worker = Wire::new(left).unwrap();
        let mut root = Wire::new(right).unwrap();
        let reply = std::thread::spawn(move || {
            let shown = root.receive(LIFETIME).unwrap();
            let mut input = Reader::new(&shown.0);
            assert_eq!(input.byte().unwrap(), 0x40);
            let description = input.field(256).unwrap();
            let parsed = Request::decode(description).unwrap();
            assert!(parsed.asks_pin());
            assert!(!parsed.awaits_approval());
            let mut ack = Bytes(vec![0x41]);
            field(&mut ack, description).unwrap();
            root.send(&ack.0).unwrap();
            let mut pin = Bytes(vec![0x42]);
            // A valid PIN paired with the wrong description must be refused.
            let mut wrong = description.to_vec();
            wrong[10] ^= 1;
            field(&mut pin, &wrong).unwrap();
            field(&mut pin, b"123456").unwrap();
            root.send(&pin.0).unwrap();
        });
        assert!(prompt(&mut worker, &mut request, PinPurpose::EnrollmentProof).is_err());
        assert!(prompt(&mut worker, &mut request, PinPurpose::Creation).is_err());
        reply.join().unwrap();
    }
}
