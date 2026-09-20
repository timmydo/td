//! Explicit manual hardware acceptance. No vault, persistence, or secret output.

use crate::fido_device::{Device, Session, MAX_LIFETIME};
use crate::fido_pin::Pin;
use crate::fido_transaction::{
    self as transaction, Assertion, Channel, Enrollment, PinPurpose, Transaction,
};
use crate::{crypto, pin_sys, pin_terminal::Console, portable::VerificationKey};
use std::fs::File;
use std::io::Read;
use std::time::Instant;

pub(super) fn run() -> Result<(), String> {
    crate::store::require_root()?;
    crate::secret_request::require_protected_memory()
        .map_err(|_| "host token check requires disabled swap and a zero core-dump soft limit")?;
    pin_sys::protect_process().map_err(|_| "disable token-check process dumps")?;
    let console = Console::open()?;
    let devices = Device::discover()?;
    let [device] = devices.as_slice() else {
        return Err("connect exactly one admitted root-owned FIDO USB token".into());
    };
    console.message(concat!(
        "Host authentication diagnostic; this is not td secure attention.\n",
        "Creating a nonresident td.invalid test credential on the connected token.\n",
        "The key will not be reset. No credential or secret will be saved or printed.\n",
        "Complete three PIN prompts and presence requests within two minutes.\n",
    ))?;
    let deadline = Instant::now()
        .checked_add(MAX_LIFETIME)
        .ok_or("token check deadline overflow")?;
    let mut random = File::open("/dev/urandom").map_err(|_| "open kernel entropy")?;
    roundtrip(
        &mut || Session::open(*device, deadline).map_err(|_| "open token check channel".into()),
        &mut |purpose| console.pin(purpose, deadline),
        &mut |bytes| {
            random
                .read_exact(bytes)
                .map_err(|_| "read kernel entropy".into())
        },
    )?;
    if Instant::now() >= deadline {
        return Err("token check expired".into());
    }
    console.message("PASS: PIN-protected enrollment and repeat secret proof verified.\n")
}

fn challenge(
    entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    purpose: u8,
) -> Result<[u8; 32], String> {
    let mut bytes = [0; 64];
    const CONTEXT: &[u8] = b"td-secret/manual-token-check/v1";
    bytes
        .get_mut(..CONTEXT.len())
        .ok_or("challenge context")?
        .copy_from_slice(CONTEXT);
    *bytes.get_mut(31).ok_or("challenge purpose")? = purpose;
    entropy(bytes.get_mut(32..).ok_or("challenge entropy")?)?;
    Ok(crypto::digest(&bytes))
}

fn roundtrip<C: Channel>(
    open: &mut impl FnMut() -> Result<C, String>,
    prompt: &mut impl FnMut(PinPurpose) -> Result<Pin, String>,
    entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
) -> Result<(), String> {
    let creation = challenge(entropy, 1)?;
    let proof = challenge(entropy, 2)?;
    let repeat = challenge(entropy, 3)?;
    let mut user = [0; 32];
    let mut salt = [0; 32];
    entropy(&mut user)?;
    entropy(&mut salt)?;
    let enrolled = Transaction::new(open()?)
        .map_err(diagnostic)?
        .enroll(
            Enrollment {
                challenge: creation,
                user,
                proof_challenge: proof,
                salt,
                excluded: &[],
            },
            prompt,
            entropy,
        )
        .map_err(diagnostic)?;
    let key = VerificationKey::from_cose(enrolled.cose())?.public_key()?;
    let output = Transaction::new(open()?)
        .map_err(diagnostic)?
        .assertion(
            Assertion {
                credential: enrolled.id(),
                key,
                challenge: repeat,
                salt: *enrolled.salt(),
            },
            prompt,
            entropy,
        )
        .map_err(diagnostic)?;
    verify_repeat(
        enrolled.output().bytes(),
        output.bytes(),
        enrolled.output().info.counter,
        output.info.counter,
    )
}

fn verify_repeat(first: &[u8], second: &[u8], before: u32, after: u32) -> Result<(), String> {
    if first.len() != 32 || second.len() != 32 {
        return Err("invalid token secret length".into());
    }
    let difference = first
        .iter()
        .zip(second)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b));
    if std::hint::black_box(difference) != 0 {
        return Err("token secret was not repeatable".into());
    }
    if (before != 0 || after != 0) && after <= before {
        return Err("token assertion counter did not advance".into());
    }
    Ok(())
}

fn diagnostic(error: transaction::Error) -> String {
    use transaction::Error;
    match error {
        Error::Status(status) => format!("token refused: {status:?}; no automatic retry"),
        Error::Interrupted(reason) => format!("token check interrupted: {reason:?}"),
        Error::Transport => {
            "token transport failed; device outcome uncertain; no automatic retry".into()
        }
        Error::Protocol(_) => "token capability or protocol verification refused".into(),
        Error::PinInput => "PIN input cancelled, expired, invalid, or terminal unavailable".into(),
        Error::Entropy => "kernel entropy unavailable".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fido_device::Interruption;
    use crate::fido_hid::{self as hid, Event, Message};
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    fn fixture(label: &str, field: &str) -> Vec<u8> {
        let row = include_str!("../tests/token_check_vectors.txt")
            .lines()
            .map(|row| row.split_whitespace().collect::<Vec<_>>())
            .find(|row| row.first() == Some(&label) && row.get(1) == Some(&field))
            .unwrap();
        row[2]
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    struct Script {
        steps: VecDeque<(Vec<u8>, Vec<u8>)>,
        commands: Rc<Cell<usize>>,
        drops: Rc<Cell<usize>>,
        fail: Option<usize>,
    }
    impl Channel for Script {
        fn check(&self) -> Result<(), Interruption> {
            Ok(())
        }
        fn exchange(&mut self, bytes: &[u8]) -> Result<Message, String> {
            let index = self.commands.get();
            self.commands.set(index + 1);
            let (request, mut response) = self.steps.pop_front().expect("command replay");
            assert_eq!(bytes, request);
            if self.fail == Some(index) {
                response = vec![0x31];
            }
            let mut decoder = hid::Decoder::cbor(1).unwrap();
            for report in hid::cbor(1, &response).unwrap().as_ref() {
                if let Event::Complete(message) = decoder.push(report).unwrap() {
                    return Ok(message);
                }
            }
            panic!("incomplete public fixture");
        }
    }
    impl Drop for Script {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    fn exercise(
        label: &str,
        response: &str,
        fail: Option<usize>,
    ) -> (Result<(), String>, usize, usize, Vec<PinPurpose>) {
        let mut entropy_fields = VecDeque::from([
            "creation_entropy",
            "proof_entropy",
            "repeat_entropy",
            "user",
            "salt",
            "create_scalar",
        ]);
        if label.starts_with("p2") {
            entropy_fields.push_back("create_iv_pin");
        }
        entropy_fields.push_back("scalar");
        if label.starts_with("p2") {
            entropy_fields.extend(["iv_pin", "iv_salt"]);
        }
        entropy_fields.push_back("repeat_scalar");
        if label.starts_with("p2") {
            entropy_fields.extend(["repeat_iv_pin", "repeat_iv_salt"]);
        }
        let commands = Rc::new(Cell::new(0));
        let drops = Rc::new(Cell::new(0));
        let mut opens = 0;
        let mut purposes = Vec::new();
        let result = roundtrip(
            &mut || {
                assert!(opens < 2, "channel replay");
                let pairs = if opens == 0 {
                    vec![
                        ("key_request", "create_key_response"),
                        ("create_pin_request", "create_pin_response"),
                        ("make_request", "make_none"),
                        ("key_request", "key_response"),
                        ("pin_request", "pin_response"),
                        ("enroll_assertion", "enroll_response"),
                    ]
                } else {
                    vec![
                        ("key_request", "repeat_key_response"),
                        ("repeat_pin_request", "repeat_pin_response"),
                        ("repeat_assertion", response),
                    ]
                };
                opens += 1;
                let mut steps = VecDeque::from([(vec![4], fixture(label, "info"))]);
                steps.extend(
                    pairs
                        .into_iter()
                        .map(|(a, b)| (fixture(label, a), fixture(label, b))),
                );
                Ok(Script {
                    steps,
                    commands: commands.clone(),
                    drops: drops.clone(),
                    fail,
                })
            },
            &mut |purpose| {
                purposes.push(purpose);
                Pin::new(fixture(label, "pin").into_boxed_slice())
            },
            &mut |out| {
                out.copy_from_slice(&fixture(
                    label,
                    entropy_fields.pop_front().expect("extra entropy"),
                ));
                Ok(())
            },
        );
        assert_eq!(drops.get(), opens);
        (result, commands.get(), drops.get(), purposes)
    }

    #[test]
    fn independent_complete_transcripts_prove_repeat_and_reject_signed_wrong_or_stale_results() {
        for label in ["p1-legacy", "p1-scoped", "p2-legacy", "p2-scoped"] {
            for response in ["repeat_response", "repeat_wrong", "repeat_stale"] {
                let (result, commands, drops, purposes) = exercise(label, response, None);
                assert_eq!(commands, 11);
                assert_eq!(drops, 2);
                assert_eq!(
                    purposes,
                    [
                        PinPurpose::Creation,
                        PinPurpose::EnrollmentProof,
                        PinPurpose::Assertion
                    ]
                );
                match response {
                    "repeat_response" => result.unwrap(),
                    "repeat_wrong" => {
                        assert_eq!(result.unwrap_err(), "token secret was not repeatable")
                    }
                    _ => assert_eq!(
                        result.unwrap_err(),
                        "token assertion counter did not advance"
                    ),
                }
            }
        }
        for failure in 0..11 {
            let (result, commands, drops, _) =
                exercise("p2-scoped", "repeat_response", Some(failure));
            assert!(result.unwrap_err().contains("PinInvalid"));
            assert_eq!(commands, failure + 1);
            assert_eq!(drops, if failure < 7 { 1 } else { 2 });
        }
    }

    #[test]
    fn repeat_requires_the_same_secret_and_a_valid_counter_transition() {
        for (before, after, accepted) in [
            (0, 0, true),
            (0, 1, true),
            (7, 8, true),
            (7, 7, false),
            (7, 0, false),
            (8, 7, false),
            (u32::MAX, 1, false),
        ] {
            assert_eq!(
                verify_repeat(&[7; 32], &[7; 32], before, after).is_ok(),
                accepted
            );
        }
        for index in 0..32 {
            let mut second = [7; 32];
            second[index] ^= 1;
            assert!(verify_repeat(&[7; 32], &second, 0, 0).is_err());
        }
        assert!(verify_repeat(&[], &[], 0, 0).is_err());
        assert!(verify_repeat(&[7; 31], &[7; 32], 0, 0).is_err());
    }
    #[test]
    fn challenges_bind_each_phase_and_diagnostics_hide_private_text() {
        let mut entropy = |bytes: &mut [u8]| {
            bytes.fill(7);
            Ok(())
        };
        let a = challenge(&mut entropy, 1).unwrap();
        let b = challenge(&mut entropy, 2).unwrap();
        let c = challenge(&mut entropy, 3).unwrap();
        assert!(a != b && a != c && b != c);
        assert!(challenge(&mut |_| Err("entropy refused".into()), 1).is_err());
        assert_eq!(
            diagnostic(transaction::Error::Protocol("private input".into())),
            "token capability or protocol verification refused"
        );
    }
}
