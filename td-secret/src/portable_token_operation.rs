//! Root's one bounded notebook token exchange, on an inherited capability.

use super::lifecycle::{AssertRequest, EnrollRequest, Hardware, Presented, Purpose, Tokens};
use super::{host, Role, VerificationKey, MAX_CREDENTIAL, MAX_SLOTS};
use crate::consent::{Notebook, Operation, Request};
use crate::fido_device::{Device, Session};
use crate::fido_pin::Pin;
use crate::fido_transaction::PinPurpose;
use crate::notebook_wire::{field, Bytes, Reader, Wire};
use std::time::{Duration, Instant};

const LIFETIME: Duration = Duration::from_secs(120);

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
        key: VerificationKey,
        salt: [u8; 32],
        challenge: [u8; 32],
    },
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
                    key,
                    salt: input.array()?,
                    challenge: input.array()?,
                }
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

    fn execute(&self, tokens: &mut impl Tokens) -> Result<Bytes, String> {
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
                            challenge: *challenge,
                            proof_challenge: *proof,
                            user: *user,
                            salt: *salt,
                            excluded,
                        },
                    )
                    .map_err(|_| "notebook token enrollment failed")?;
                out.push(0x30)?;
                field(&mut out, &enrolled.credential)?;
                field(&mut out, enrolled.key.cose())?;
                out.extend(&enrolled.salt)?;
                out.extend(&enrolled.secret.0)?;
                out.extend(&enrolled.counter.to_be_bytes())?;
            }
            Token::Assert {
                credential,
                key,
                salt,
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
                            key,
                            salt,
                            challenge: *challenge,
                        },
                    )
                    .map_err(|_| "notebook token assertion failed")?;
                out.push(0x31)?;
                out.extend(&asserted.secret.0)?;
                out.extend(&asserted.counter.to_be_bytes())?;
            }
        }
        Ok(out)
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
    let bytes = input.field(63)?;
    input.finish()?;
    if bytes.len() < 4 || !bytes.iter().all(|b| (0x20..=0x7e).contains(b)) {
        return Err("notebook PIN is outside its profile".into());
    }
    Pin::new(bytes.to_vec().into_boxed_slice()).map_err(|_| "invalid notebook PIN".into())
}

pub(crate) fn run(owner: u32) -> Result<(), String> {
    let endpoint = crate::operation::startup()?;
    let deadline = Instant::now()
        .checked_add(LIFETIME)
        .ok_or("notebook deadline overflow")?;
    let protected = host::protect_service()?;
    let mut wire = Wire::new(endpoint)?;
    wire.bound(deadline);
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
    let devices = Device::discover()?;
    let [device] = devices.as_slice() else {
        return Err("notebook operation requires exactly one security key".into());
    };
    let device = *device;
    let mut entropy = host::Entropy::open()?;
    let mut tokens = Hardware {
        open: |_: Presented<'_>| {
            Session::open(device, deadline).map_err(|_| super::lifecycle::TokenError::Unavailable)
        },
        prompt: |_: Presented<'_>, purpose| prompt(&mut wire, &mut request, purpose),
        entropy: |bytes: &mut [u8]| entropy.fill(bytes),
    };
    let result = exchange.execute(&mut tokens)?;
    protected.recheck()?;
    let mut out = Bytes(vec![0x44]);
    field(&mut out, &result.0)?;
    wire.send(&out.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

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
                let result = exchange.execute(&mut bench)?;
                methods.push(bytes.0[0]);
                wire.send(&result.0)?;
            }
            Ok(methods)
        });
        let mut wire = Wire::new(left).unwrap();
        let mut remote = super::super::service_tokens::Remote::new(&mut wire);
        let result = lifecycle::create(&directory, &mut remote, &mut Random(0), true);
        let handled = root.join().unwrap();
        assert!(result.is_ok(), "{handled:?}");
        assert_eq!(handled.unwrap(), [0x30, 0x31, 0x30, 0x31]);
        assert_eq!(lifecycle::keys(&directory).unwrap().len(), 2);
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
