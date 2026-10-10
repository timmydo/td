// The virtual authenticator's own tests: the protocol paths it serves,
// driven through the client transactions, and its persisted state. Only
// this crate compiles them; td-secret's tests compile the authenticator.

use super::*;
use crate::fido_ctap::signature;
use crate::fido_fixtures::fixture;
use crate::fido_pin::LoginRefusal;
use crate::fido_pin::{EnrolledCredential, HmacOutput, Pin};
use crate::fido_transaction::{
    Assertion, Enrollment, Error, LoginAssertion, LoginCreation, LoginError, LoginPin, Status,
    Transaction,
};
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

fn create(key: &Virtual, excluded: &[&[u8]], seed: u8) -> Result<EnrolledCredential, LoginError> {
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

    // A wrong secret still verifies its signature; td-secret's login record
    // refuses it (login_record.rs's check tests).
    let output: [u8; 32] = created.output().bytes().try_into().unwrap();
    key.script(Script {
        output: Output::Wrong,
        ..Script::default()
    });
    let wrong = login(&key, &created, PIN, &mut Vec::new()).unwrap();
    let wrong: [u8; 32] = wrong.bytes().try_into().unwrap();
    assert_ne!(wrong, output);

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
