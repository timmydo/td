//! Owned portable PIN transactions. Transport admission and presentation stay backend duties.

use super::fido_ctap::{IdentifyRequest, MAX_CREDENTIAL_ID};
use super::fido_device::{Interruption, Session};
use super::fido_hid::Message;
use super::fido_p256::PublicKey;
use super::fido_pin::{
    Creation, EnrolledCredential, HmacOutput, KeyRequest, LoginFailure, LoginRefusal, Pin, Profile,
    RetriesRequest,
};

/// A trusted backend supplies one channel and its original lifetime throughout.
/// Drop must retire the channel; exchange must observe the same revocation state.
pub(super) trait Channel {
    fn check(&self) -> Result<(), Interruption>;
    fn exchange(&mut self, request: &[u8]) -> Result<Message, String>;
}

impl Channel for Session {
    fn check(&self) -> Result<(), Interruption> {
        self.check_active()
    }
    fn exchange(&mut self, request: &[u8]) -> Result<Message, String> {
        self.cbor(request)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
    CredentialExcluded,
    Denied,
    Cancelled,
    NoCredential,
    TouchTimeout,
    PinInvalid,
    PinBlocked,
    PinAuthInvalid,
    PinAuthBlocked,
    PinNotSet,
    PinRequired,
    PinPolicy,
    ActionTimeout,
    Other(u8),
}
impl Status {
    fn decode(byte: u8) -> Self {
        match byte {
            0x19 => Self::CredentialExcluded,
            0x27 => Self::Denied,
            0x2d => Self::Cancelled,
            0x2e => Self::NoCredential,
            0x2f => Self::TouchTimeout,
            0x31 => Self::PinInvalid,
            0x32 => Self::PinBlocked,
            0x33 => Self::PinAuthInvalid,
            0x34 => Self::PinAuthBlocked,
            0x35 => Self::PinNotSet,
            0x36 => Self::PinRequired,
            0x37 => Self::PinPolicy,
            0x3a => Self::ActionTimeout,
            other => Self::Other(other),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Error {
    Interrupted(Interruption),
    Transport,
    Status(Status),
    Protocol(String),
    PinInput,
    Entropy,
}

/// Login keeps portable's typed failures and adds TOKEN-LOGIN.md's capability refusals.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum LoginError {
    Refused(LoginRefusal),
    Failed(Error),
}
impl From<Error> for LoginError {
    fn from(error: Error) -> Self {
        Self::Failed(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PinPurpose {
    Assertion,
    Creation,
    EnrollmentProof,
}

pub(super) struct Assertion<'a> {
    pub credential: &'a [u8],
    pub key: PublicKey,
    pub challenge: [u8; 32],
    pub salt: [u8; 32],
}
/// A login assertion's enrolled credential. Its client-data hash comes from
/// the prompt, which presents the step naming the key's reported retries.
pub(super) struct LoginAssertion<'a> {
    pub credential: &'a [u8],
    pub key: PublicKey,
    pub salt: [u8; 32],
}
pub(super) struct Enrollment<'a> {
    pub challenge: [u8; 32],
    pub user: [u8; 32],
    pub proof_challenge: [u8; 32],
    pub salt: [u8; 32],
    pub excluded: &'a [&'a [u8]],
}

fn status(reply: &Message) -> Result<u8, Error> {
    reply
        .as_ref()
        .first()
        .copied()
        .ok_or_else(|| Error::Protocol("missing portable CTAP status".into()))
}

/// Single-use: success and every failure drop the owned channel.
pub(super) struct Transaction<C: Channel> {
    channel: C,
}
impl<C: Channel> Transaction<C> {
    pub(super) fn new(channel: C) -> Result<Self, Error> {
        let transaction = Self { channel };
        transaction.check()?;
        Ok(transaction)
    }

    fn check(&self) -> Result<(), Error> {
        self.channel.check().map_err(Error::Interrupted)
    }

    fn transition<T>(&self, result: Result<T, String>) -> Result<T, Error> {
        // A late successful transition must retire just like a late wire reply.
        self.check()?;
        result.map_err(Error::Protocol)
    }

    fn command(&mut self, bytes: &[u8]) -> Result<Message, Error> {
        let reply = self.exchange(bytes)?;
        let status = status(&reply)?;
        if status != 0 {
            return Err(Error::Status(Status::decode(status)));
        }
        Ok(reply)
    }

    /// One wire exchange whose reply may carry any status.
    fn exchange(&mut self, bytes: &[u8]) -> Result<Message, Error> {
        self.check()?;
        let reply = self.channel.exchange(bytes);
        let active = self.channel.check();
        if let Err(reason @ (Interruption::Cancelled | Interruption::Expired)) = active {
            return Err(Error::Interrupted(reason));
        }
        let reply = reply.map_err(|_| Error::Transport)?;
        active.map_err(Error::Interrupted)?;
        Ok(reply)
    }

    fn profile(&mut self) -> Result<Profile, Error> {
        let reply = self.command(&[4])?;
        self.transition(Profile::parse(reply.as_ref()))
    }

    fn collect_pin(
        &self,
        purpose: PinPurpose,
        prompt: &mut impl FnMut(PinPurpose) -> Result<Pin, String>,
    ) -> Result<Pin, Error> {
        self.check()?;
        let pin = prompt(purpose);
        self.check()?;
        pin.map_err(|_| Error::PinInput)
    }

    fn with_entropy<T>(
        &self,
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
        step: impl FnOnce(&mut dyn FnMut(&mut [u8]) -> Result<(), String>) -> Result<T, String>,
    ) -> Result<T, Error> {
        self.check()?;
        let mut failed = false;
        let result = step(&mut |bytes| {
            self.check()
                .map_err(|_| "portable operation inactive".to_string())?;
            if entropy(bytes).is_err() {
                failed = true;
                return Err("portable entropy unavailable".into());
            }
            self.check()
                .map_err(|_| "portable operation inactive".to_string())
        });
        self.check()?;
        if failed {
            return Err(Error::Entropy);
        }
        result.map_err(Error::Protocol)
    }

    pub(super) fn assertion(
        mut self,
        intent: Assertion<'_>,
        prompt: &mut impl FnMut(PinPurpose) -> Result<Pin, String>,
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<HmacOutput, Error> {
        let profile = self.profile()?;
        let request = self.transition(profile.assertion(
            intent.credential,
            intent.key,
            intent.challenge,
            intent.salt,
        ))?;
        let output = self.authorized_assertion(
            request,
            &mut |run, purpose| run.collect_pin(purpose, prompt),
            entropy,
        )?;
        if output.info.backup_eligible || output.info.backed_up {
            return Err(Error::Protocol(
                "portable assertion is not device-bound".into(),
            ));
        }
        self.check()?;
        Ok(output)
    }

    pub(super) fn enroll(
        mut self,
        intent: Enrollment<'_>,
        prompt: &mut impl FnMut(PinPurpose) -> Result<Pin, String>,
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<EnrolledCredential, Error> {
        self.check()?;
        if intent.challenge == intent.proof_challenge {
            return Err(Error::Protocol(
                "enrollment proof reuses creation challenge".into(),
            ));
        }
        let profile = self.profile()?;
        let request =
            self.transition(profile.enrollment(intent.challenge, intent.user, intent.excluded))?;
        let output = self.create_and_prove(
            request,
            &intent,
            &mut |run, purpose| run.collect_pin(purpose, prompt),
            entropy,
        )?;
        self.check()?;
        Ok(output)
    }

    /// Key agreement, then the PIN, its token and the signed hmac-secret assertion.
    fn authorized_assertion(
        &mut self,
        request: KeyRequest,
        pin: &mut impl FnMut(&mut Self, PinPurpose) -> Result<Pin, Error>,
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<HmacOutput, Error> {
        let reply = self.command(request.bytes())?;
        let pin = pin(self, PinPurpose::Assertion)?;
        self.pin_assertion(request, reply, pin, entropy)
    }

    /// The PIN token and the signed hmac-secret assertion after key agreement.
    fn pin_assertion(
        &mut self,
        request: KeyRequest,
        reply: Message,
        pin: Pin,
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<HmacOutput, Error> {
        let request = self.with_entropy(entropy, |entropy| {
            request.with_pin(reply.as_ref(), pin, &mut |bytes| entropy(bytes))
        })?;
        drop(reply);
        let reply = self.command(request.bytes())?;
        let request = self.with_entropy(entropy, |entropy| {
            request.finish(reply.as_ref(), &mut |bytes| entropy(bytes))
        })?;
        drop(reply);
        let reply = self.command(request.bytes())?;
        self.transition(request.finish(reply.as_ref()))
    }

    /// Creation under its own PIN, then a fresh PIN-authorized proof of the new key.
    fn create_and_prove(
        &mut self,
        request: KeyRequest<Creation>,
        intent: &Enrollment<'_>,
        pin: &mut impl FnMut(&mut Self, PinPurpose) -> Result<Pin, Error>,
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<EnrolledCredential, Error> {
        let reply = self.command(request.bytes())?;
        let pin_input = pin(self, PinPurpose::Creation)?;
        let request = self.with_entropy(entropy, |entropy| {
            request.with_pin(reply.as_ref(), pin_input, &mut |bytes| entropy(bytes))
        })?;
        drop(reply);
        let reply = self.command(request.bytes())?;
        let request = self.transition(request.make(reply.as_ref()))?;
        drop(reply);
        let reply = self.command(request.bytes())?;
        let request =
            self.transition(request.proof(reply.as_ref(), intent.proof_challenge, intent.salt))?;
        drop(reply);
        let reply = self.command(request.bytes())?;
        let pin_input = pin(self, PinPurpose::EnrollmentProof)?;
        let request = self.with_entropy(entropy, |entropy| {
            request.with_pin(reply.as_ref(), pin_input, &mut |bytes| entropy(bytes))
        })?;
        drop(reply);
        let reply = self.command(request.bytes())?;
        let request = self.with_entropy(entropy, |entropy| {
            request.finish(reply.as_ref(), &mut |bytes| entropy(bytes))
        })?;
        drop(reply);
        let reply = self.command(request.bytes())?;
        self.transition(request.finish(reply.as_ref()))
    }

    fn login_transition<T>(&self, result: Result<T, LoginFailure>) -> Result<T, LoginError> {
        self.check()?;
        result.map_err(|failure| match failure {
            LoginFailure::Refused(refusal) => LoginError::Refused(refusal),
            LoginFailure::Protocol(error) => LoginError::Failed(Error::Protocol(error)),
        })
    }

    fn login_profile(&mut self) -> Result<Profile, LoginError> {
        let reply = self.command(&[4])?;
        self.login_transition(Profile::parse_login(reply.as_ref()))
    }

    /// getPINRetries immediately before each prompt; a key with none left gets no prompt.
    fn login_pin<T>(
        &mut self,
        purpose: PinPurpose,
        retries: &RetriesRequest,
        prompt: &mut impl FnMut(PinPurpose, u8) -> Result<T, String>,
    ) -> Result<T, Error> {
        let reply = self.command(retries.bytes())?;
        let claim = self.transition(retries.parse(reply.as_ref()))?;
        drop(reply);
        if claim.count == 0 {
            return Err(Error::Status(Status::PinBlocked));
        }
        if claim.power_cycle {
            return Err(Error::Status(Status::PinAuthBlocked));
        }
        let pin = prompt(purpose, claim.count);
        self.check()?;
        pin.map_err(|_| Error::PinInput)
    }

    /// Silent selection before any PIN, in batches within the advertised list
    /// limit (one when absent) and the message limit. None only when every batch
    /// answers a bare NO_CREDENTIALS.
    pub(super) fn identify(
        mut self,
        credentials: &[&[u8]],
        challenge: [u8; 32],
    ) -> Result<Option<usize>, LoginError> {
        self.check()?;
        if credentials.is_empty() {
            return Err(Error::Protocol("identify requires an enrolled credential".into()).into());
        }
        for (index, id) in credentials.iter().enumerate() {
            if id.is_empty()
                || id.len() > MAX_CREDENTIAL_ID
                || credentials
                    .get(..index)
                    .is_some_and(|seen| seen.contains(id))
            {
                return Err(Error::Protocol("invalid identify credential list".into()).into());
            }
        }
        let profile = self.login_profile()?;
        let limit = profile.advertised_list_limit().unwrap_or(1);
        let ids = |batch: &[usize]| -> Vec<&[u8]> {
            batch
                .iter()
                .filter_map(|index| credentials.get(*index).copied())
                .collect()
        };
        // Validated IDs leave the message limit as the only reason a batch fails.
        let mut batches: Vec<Vec<usize>> = Vec::new();
        for (index, id) in credentials.iter().enumerate() {
            // One this key could not identify alone cannot be its credential.
            if !profile.identifiable(id) {
                continue;
            }
            let fits = |batch: &Vec<usize>| {
                let mut grown = ids(batch);
                grown.push(id);
                batch.len() < limit
                    && IdentifyRequest::new(&grown, challenge, profile.max_message()).is_ok()
            };
            match batches.last_mut() {
                Some(batch) if fits(batch) => batch.push(index),
                _ => batches.push(vec![index]),
            }
        }
        for batch in batches {
            let request = self.transition(IdentifyRequest::new(
                &ids(&batch),
                challenge,
                profile.max_message(),
            ))?;
            let reply = self.exchange(request.bytes())?;
            // Only a NO_CREDENTIALS answer goes to the codec beside success.
            let status = status(&reply)?;
            if !matches!(status, 0 | 0x2e) {
                return Err(Error::Status(Status::decode(status)).into());
            }
            let Some(position) = self.transition(request.select(reply.as_ref()))? else {
                continue;
            };
            let index = batch
                .get(position)
                .copied()
                .ok_or_else(|| Error::Protocol("identify selection extent".into()))?;
            self.check()?;
            return Ok(Some(index));
        }
        self.check()?;
        Ok(None)
    }

    /// One credential's PIN-authorized hmac-secret assertion: unlock, authorize,
    /// repeat. The prompt returns the PIN and the client-data hash of the step
    /// it presented with the reported retries.
    pub(super) fn login_assertion(
        mut self,
        intent: LoginAssertion<'_>,
        prompt: &mut impl FnMut(PinPurpose, u8) -> Result<(Pin, [u8; 32]), String>,
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<HmacOutput, LoginError> {
        let profile = self.login_profile()?;
        let retries = self.transition(profile.retries())?;
        let request =
            self.transition(profile.login_assertion(intent.credential, intent.key, intent.salt))?;
        let reply = self.command(request.bytes())?;
        let (pin, challenge) = self.login_pin(PinPurpose::Assertion, &retries, prompt)?;
        let request = self.transition(request.bind(challenge))?;
        let output = self.pin_assertion(request, reply, pin, entropy)?;
        if output.info.backup_eligible || output.info.backed_up {
            return Err(Error::Protocol("login assertion is not device-bound".into()).into());
        }
        self.check()?;
        Ok(output)
    }

    /// Creation with login labels and a whole exclusion list, then its proof.
    pub(super) fn login_create(
        mut self,
        intent: Enrollment<'_>,
        prompt: &mut impl FnMut(PinPurpose, u8) -> Result<Pin, String>,
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<EnrolledCredential, LoginError> {
        self.check()?;
        if intent.challenge == intent.proof_challenge {
            return Err(Error::Protocol("login proof reuses creation challenge".into()).into());
        }
        let profile = self.login_profile()?;
        let retries = self.transition(profile.retries())?;
        let request = self.login_transition(profile.login_enrollment(
            intent.challenge,
            intent.user,
            intent.excluded,
        ))?;
        let output = self.create_and_prove(
            request,
            &intent,
            &mut |run, purpose| run.login_pin(purpose, &retries, prompt),
            entropy,
        )?;
        self.check()?;
        Ok(output)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fido_device::{self as device, Cancellation};
    use crate::fido_hid::{self as hid, Event};
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::rc::Rc;
    use std::time::Duration;

    const LABELS: [&str; 4] = ["p1-legacy", "p1-scoped", "p2-legacy", "p2-scoped"];

    pub(crate) fn fixture(label: &str, name: &str) -> Vec<u8> {
        let row = include_str!("../tests/pin_vectors.txt")
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
            .find(|row| row.first() == Some(&label) && row.get(1) == Some(&name))
            .unwrap();
        row[2]
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
    pub(crate) fn login_fixture(label: &str, name: &str) -> Vec<u8> {
        let row = include_str!("../tests/login_ctap_vectors.txt")
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
            .find(|row| row.first() == Some(&label) && row.get(1) == Some(&name))
            .unwrap();
        row[2]
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    /// getInfo claims rewritten into a label's committed getInfo row.
    pub(crate) struct Info {
        pub(crate) extensions: Extensions,
        /// False omits the whole options map.
        pub(crate) options: bool,
        pub(crate) client_pin: Option<bool>,
        pub(crate) always_uv: Option<bool>,
        pub(crate) list: Option<u8>,
        pub(crate) max_id: Option<u8>,
    }
    #[derive(Clone, Copy, PartialEq)]
    pub(crate) enum Extensions {
        Hmac,
        Other,
        Empty,
        Absent,
    }
    impl Default for Info {
        fn default() -> Self {
            Self {
                extensions: Extensions::Hmac,
                options: true,
                client_pin: Some(true),
                always_uv: None,
                list: None,
                max_id: None,
            }
        }
    }
    pub(crate) fn info_with(label: &str, info: Info) -> Vec<u8> {
        let mut bytes = fixture(label, "info");
        // The committed extensions are 2: ["hmac-secret"].
        let at = bytes
            .windows(14)
            .position(|s| s == b"\x02\x81\x6bhmac-secret")
            .unwrap();
        match info.extensions {
            Extensions::Hmac => {}
            Extensions::Other => {
                bytes[at + 2..at + 14].copy_from_slice(b"\x6bcredProtect");
            }
            Extensions::Empty => {
                bytes.splice(at + 1..at + 14, [0x80]);
            }
            Extensions::Absent => {
                bytes.drain(at..at + 14);
                bytes[1] -= 1;
            }
        }
        // The committed options are {clientPin: true, pinUvAuthToken: P}.
        let start = bytes
            .windows(10)
            .position(|s| s == b"\x69clientPin")
            .unwrap()
            - 1;
        let end = start + 1 + 10 + 1 + 15 + 1;
        let permissions = bytes[end - 1] == 0xf5;
        let mut options: Vec<(&str, bool)> = Vec::new();
        options.extend(info.always_uv.map(|value| ("alwaysUv", value)));
        options.extend(info.client_pin.map(|value| ("clientPin", value)));
        options.push(("pinUvAuthToken", permissions));
        let mut encoded = vec![0xa0 + options.len() as u8];
        for (name, value) in options {
            encoded.push(0x60 + name.len() as u8);
            encoded.extend(name.as_bytes());
            encoded.push(if value { 0xf5 } else { 0xf4 });
        }
        if info.options {
            bytes.splice(start..end, encoded);
        } else {
            bytes.drain(start - 1..end);
            bytes[1] -= 1;
        }
        for (key, value) in [(7, info.list), (8, info.max_id)] {
            if let Some(value) = value {
                bytes[1] += 1;
                bytes.push(key);
                if value >= 24 {
                    bytes.push(0x18);
                }
                bytes.push(value);
            }
        }
        bytes
    }

    pub(crate) fn assertion(label: &str) -> Assertion<'static> {
        Assertion {
            credential: b"fixture-id",
            key: PublicKey::from_coordinates(
                &fixture(label, "x").try_into().unwrap(),
                &fixture(label, "y").try_into().unwrap(),
            )
            .unwrap(),
            challenge: fixture(label, "challenge").try_into().unwrap(),
            salt: fixture(label, "salt").try_into().unwrap(),
        }
    }
    /// The fixture assertion as a login intent and the hash its prompt returns.
    pub(crate) fn login_intent(label: &str) -> (LoginAssertion<'static>, [u8; 32]) {
        let Assertion {
            credential,
            key,
            challenge,
            salt,
        } = assertion(label);
        (
            LoginAssertion {
                credential,
                key,
                salt,
            },
            challenge,
        )
    }
    pub(crate) fn enrollment(label: &str) -> Enrollment<'static> {
        Enrollment {
            challenge: fixture(label, "create_challenge").try_into().unwrap(),
            user: fixture(label, "user").try_into().unwrap(),
            proof_challenge: fixture(label, "challenge").try_into().unwrap(),
            salt: fixture(label, "salt").try_into().unwrap(),
            excluded: &[],
        }
    }
    pub(crate) fn entropy(
        label: &str,
        creation: bool,
    ) -> impl FnMut(&mut [u8]) -> Result<(), String> + '_ {
        let mut fields = VecDeque::new();
        if creation {
            fields.push_back("create_scalar");
            if label.starts_with("p2") {
                fields.push_back("create_iv_pin");
            }
        }
        fields.push_back("scalar");
        if label.starts_with("p2") {
            fields.extend(["iv_pin", "iv_salt"]);
        }
        move |out| {
            out.copy_from_slice(&fixture(label, fields.pop_front().expect("extra entropy")));
            Ok(())
        }
    }
    fn transcript(label: &str, creation: bool) -> VecDeque<(Vec<u8>, Vec<u8>)> {
        let mut steps = VecDeque::from([(vec![4], fixture(label, "info"))]);
        if creation {
            steps.extend([
                (
                    fixture(label, "key_request"),
                    fixture(label, "create_key_response"),
                ),
                (
                    fixture(label, "create_pin_request"),
                    fixture(label, "create_pin_response"),
                ),
                (fixture(label, "make_request"), fixture(label, "make_none")),
            ]);
        }
        steps.extend([
            (
                fixture(label, "key_request"),
                fixture(label, "key_response"),
            ),
            (
                fixture(label, "pin_request"),
                fixture(label, "pin_response"),
            ),
            (
                fixture(
                    label,
                    if creation {
                        "enroll_assertion"
                    } else {
                        "assertion"
                    },
                ),
                fixture(
                    label,
                    if creation {
                        "enroll_response"
                    } else {
                        "response"
                    },
                ),
            ),
        ]);
        steps
    }
    pub(crate) fn message(bytes: &[u8]) -> Message {
        let mut decoder = hid::Decoder::cbor(1).unwrap();
        for report in hid::cbor(1, bytes).unwrap().as_ref() {
            if let Event::Complete(message) = decoder.push(report).unwrap() {
                return message;
            }
        }
        panic!("incomplete fixture message")
    }

    // Invoked only by the owned, re-executed fido_device test worker.
    pub(crate) fn worker(role: &str, input: &mut impl Read, output: &mut impl Write) {
        let (label, mode) = role.split_once(':').unwrap();
        assert!(LABELS.contains(&label));
        let mut steps = transcript(label, mode == "enroll");
        assert!(["assert", "enroll"].contains(&mode));
        let mut decoder = hid::Decoder::cbor(1).unwrap();
        let mut reports = VecDeque::new();
        let mut keepalive = false;
        let mut op = [0];
        while input.read_exact(&mut op).is_ok() {
            match op {
                [1] => {
                    assert!(reports.is_empty());
                    let mut report = [0; 64];
                    input.read_exact(&mut report).unwrap();
                    if let Event::Complete(request) = decoder.push(&report).unwrap() {
                        let (expected, response) = steps.pop_front().expect("replayed command");
                        assert_eq!(request.as_ref(), expected);
                        reports.extend(hid::cbor(1, &response).unwrap().as_ref().iter().copied());
                        decoder = hid::Decoder::cbor(1).unwrap();
                        keepalive = true;
                    }
                    output.write_all(&[0]).unwrap();
                }
                [2] if keepalive => {
                    let mut report = [0; 64];
                    report[..8].copy_from_slice(&[0, 0, 0, 1, 0xbb, 0, 1, 2]);
                    output.write_all(&report).unwrap();
                    keepalive = false;
                }
                [2] => output
                    .write_all(&reports.pop_front().expect("extra read"))
                    .unwrap(),
                _ => panic!("invalid worker operation"),
            }
            output.flush().unwrap();
        }
    }

    #[derive(Default)]
    pub(crate) struct Trace {
        pub(crate) commands: Cell<usize>,
        pub(crate) drops: Cell<usize>,
        interrupted: Cell<Option<Interruption>>,
        final_checks: Cell<usize>,
        pins: RefCell<Vec<PinPurpose>>,
    }
    pub(crate) struct Script {
        trace: Rc<Trace>,
        steps: VecDeque<(Vec<u8>, Vec<u8>)>,
        fail: Option<usize>,
        late: bool,
    }
    impl Script {
        pub(crate) fn new(label: &str, creation: bool) -> (Self, Rc<Trace>) {
            let trace = Rc::new(Trace::default());
            (
                Self {
                    trace: trace.clone(),
                    steps: transcript(label, creation),
                    fail: None,
                    late: false,
                },
                trace,
            )
        }

        fn steps(steps: Vec<(Vec<u8>, Vec<u8>)>) -> (Self, Rc<Trace>) {
            let trace = Rc::new(Trace::default());
            (
                Self {
                    trace: trace.clone(),
                    steps: steps.into(),
                    fail: None,
                    late: false,
                },
                trace,
            )
        }
    }
    impl Channel for Script {
        fn check(&self) -> Result<(), Interruption> {
            if self.late && self.trace.commands.get() == 4 {
                self.trace
                    .final_checks
                    .set(self.trace.final_checks.get() + 1);
                if self.trace.final_checks.get() == 2 {
                    self.trace.interrupted.set(Some(Interruption::Cancelled));
                }
            }
            self.trace.interrupted.get().map_or(Ok(()), Err)
        }
        fn exchange(&mut self, request: &[u8]) -> Result<Message, String> {
            let step = self.trace.commands.get();
            self.trace.commands.set(step + 1);
            let (expected, response) = self.steps.pop_front().expect("automatic retry");
            assert_eq!(request, expected);
            if self.fail == Some(step) {
                return Err("uncertain transport outcome".into());
            }
            Ok(message(&response))
        }
    }
    impl Drop for Script {
        fn drop(&mut self) {
            self.trace.drops.set(self.trace.drops.get() + 1);
        }
    }
    fn prompt(trace: &Trace, purpose: PinPurpose) -> Result<Pin, String> {
        trace.pins.borrow_mut().push(purpose);
        Pin::new(fixture("p2-scoped", "pin").into_boxed_slice())
    }

    #[test]
    fn both_pin_protocols_and_permissions_drive_real_worker_streams() {
        for label in LABELS {
            for creation in [false, true] {
                let mode = if creation { "enroll" } else { "assert" };
                let channel = device::tests::cancellable_fixture(
                    &format!("portable:{label}:{mode}"),
                    Duration::from_secs(30),
                    Cancellation::new(),
                );
                let run = Transaction::new(channel).unwrap();
                let mut purposes = Vec::new();
                let mut prompt = |purpose| {
                    purposes.push(purpose);
                    Pin::new(fixture(label, "pin").into_boxed_slice())
                };
                if creation {
                    let output = run
                        .enroll(enrollment(label), &mut prompt, &mut entropy(label, true))
                        .unwrap();
                    assert_eq!(output.id(), fixture(label, "credential_id"));
                    assert_eq!(output.cose(), fixture(label, "cose"));
                    assert_eq!(output.output().bytes(), fixture(label, "output"));
                    assert_eq!(
                        purposes,
                        [PinPurpose::Creation, PinPurpose::EnrollmentProof]
                    );
                } else {
                    let output = run
                        .assertion(assertion(label), &mut prompt, &mut entropy(label, false))
                        .unwrap();
                    assert_eq!(output.bytes(), fixture(label, "output"));
                    assert_eq!(purposes, [PinPurpose::Assertion]);
                }
            }
        }
    }

    #[test]
    fn every_nonzero_status_stops_without_a_prompt_or_a_retry() {
        for byte in 1..=u8::MAX {
            let (mut channel, trace) = Script::new("p2-scoped", false);
            channel.steps[0].1 = vec![byte];
            let result = Transaction::new(channel).unwrap().assertion(
                assertion("p2-scoped"),
                &mut |_| panic!("unexpected PIN prompt"),
                &mut |_| panic!("unexpected entropy"),
            );
            assert_eq!(result.err().unwrap(), Error::Status(Status::decode(byte)));
            assert_eq!(trace.commands.get(), 1);
            assert_eq!(trace.drops.get(), 1);
        }
        for (byte, status) in [
            (0x19, Status::CredentialExcluded),
            (0x31, Status::PinInvalid),
            (0x32, Status::PinBlocked),
            (0x33, Status::PinAuthInvalid),
            (0x34, Status::PinAuthBlocked),
            (0x35, Status::PinNotSet),
            (0x36, Status::PinRequired),
            (0x37, Status::PinPolicy),
            (0x27, Status::Denied),
            (0x2d, Status::Cancelled),
            (0x2e, Status::NoCredential),
            (0x2f, Status::TouchTimeout),
            (0x3a, Status::ActionTimeout),
            (0xff, Status::Other(0xff)),
        ] {
            assert_eq!(Status::decode(byte), status);
        }
    }

    #[test]
    fn pin_refusal_and_uncertain_transport_stop_both_enrollment_phases() {
        for step in 0..7 {
            for status in [None, Some(0x31), Some(0x32), Some(0x34)] {
                let (mut channel, trace) = Script::new("p2-scoped", true);
                if let Some(status) = status {
                    channel.steps[step].1 = vec![status];
                } else {
                    channel.fail = Some(step);
                }
                let result = Transaction::new(channel).unwrap().enroll(
                    enrollment("p2-scoped"),
                    &mut |purpose| prompt(&trace, purpose),
                    &mut entropy("p2-scoped", true),
                );
                let expected =
                    status.map_or(Error::Transport, |byte| Error::Status(Status::decode(byte)));
                assert_eq!(result.err().unwrap(), expected);
                assert_eq!(trace.commands.get(), step + 1);
                assert_eq!(trace.drops.get(), 1);
                assert_eq!(
                    trace.pins.borrow().len(),
                    usize::from(step >= 2) + usize::from(step >= 5)
                );
            }
        }
    }

    #[test]
    fn revocation_at_prompt_entropy_and_verified_result_retires_everything() {
        for phase in ["early", "prompt", "entropy"] {
            for reason in [Interruption::Cancelled, Interruption::Expired] {
                let (channel, trace) = Script::new("p2-scoped", false);
                if phase == "early" {
                    trace.interrupted.set(Some(reason));
                }
                let result = Transaction::new(channel).and_then(|run| {
                    run.assertion(
                        assertion("p2-scoped"),
                        &mut |purpose| {
                            if phase == "prompt" {
                                trace.interrupted.set(Some(reason));
                            }
                            prompt(&trace, purpose)
                        },
                        &mut |bytes| {
                            if phase == "entropy" {
                                trace.interrupted.set(Some(reason));
                            }
                            bytes.fill(1);
                            Err("entropy unavailable".into())
                        },
                    )
                });
                assert_eq!(result.err().unwrap(), Error::Interrupted(reason));
                assert_eq!(trace.commands.get(), if phase == "early" { 0 } else { 2 });
                assert_eq!(trace.drops.get(), 1);
            }
        }
        let (mut channel, trace) = Script::new("p2-scoped", false);
        channel.late = true;
        assert_eq!(
            Transaction::new(channel)
                .unwrap()
                .assertion(
                    assertion("p2-scoped"),
                    &mut |purpose| prompt(&trace, purpose),
                    &mut entropy("p2-scoped", false)
                )
                .err()
                .unwrap(),
            Error::Interrupted(Interruption::Cancelled)
        );
        assert_eq!(trace.commands.get(), 4);
        assert_eq!(trace.drops.get(), 1);
    }
    #[test]
    fn concrete_transport_interruptions_remain_typed_after_worker_retirement() {
        let mut retired = device::tests::cancellable_fixture(
            "short",
            Duration::from_secs(5),
            Cancellation::new(),
        );
        assert!(retired.cbor(&[4]).is_err());
        assert_eq!(
            Transaction::new(retired).err().unwrap(),
            Error::Interrupted(Interruption::Closed)
        );
        for creation in [false, true] {
            for (role, time, expected) in [
                (
                    "stall",
                    Duration::from_secs(5),
                    Error::Interrupted(Interruption::Cancelled),
                ),
                (
                    "keepalive",
                    Duration::from_millis(80),
                    Error::Interrupted(Interruption::Expired),
                ),
                ("short", Duration::from_secs(5), Error::Transport),
            ] {
                let cancellation = Cancellation::new();
                let channel = device::tests::cancellable_fixture(role, time, cancellation.clone());
                let canceller = (role == "stall").then(|| {
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(100));
                        cancellation.cancel();
                    })
                });
                let result = Transaction::new(channel).and_then(|run| {
                    if creation {
                        return run
                            .enroll(
                                enrollment("p2-scoped"),
                                &mut |_| panic!("unexpected prompt"),
                                &mut |_| panic!("unexpected entropy"),
                            )
                            .map(|_| ());
                    }
                    run.assertion(
                        assertion("p2-scoped"),
                        &mut |_| panic!("unexpected prompt"),
                        &mut |_| panic!("unexpected entropy"),
                    )
                    .map(|_| ())
                });
                if let Some(canceller) = canceller {
                    canceller.join().unwrap();
                }
                assert_eq!(result.err().unwrap(), expected, "{role}");
            }
        }
    }

    #[test]
    fn reused_proof_challenge_and_local_failures_never_continue() {
        let (channel, trace) = Script::new("p2-scoped", true);
        let mut intent = enrollment("p2-scoped");
        intent.proof_challenge = intent.challenge;
        assert!(matches!(
            Transaction::new(channel).unwrap().enroll(
                intent,
                &mut |_| panic!("unexpected prompt"),
                &mut |_| panic!("unexpected entropy")
            ),
            Err(Error::Protocol(_))
        ));
        assert_eq!(trace.commands.get(), 0);
        assert_eq!(trace.drops.get(), 1);
        for phase in ["prompt", "entropy", "capabilities"] {
            let (mut channel, trace) = Script::new("p2-scoped", false);
            if phase == "capabilities" {
                channel.steps[0].1 = vec![0, 0xa0];
            }
            let result = Transaction::new(channel).unwrap().assertion(
                assertion("p2-scoped"),
                &mut |purpose| {
                    if phase == "prompt" {
                        return Err("private prompt diagnostic".into());
                    }
                    prompt(&trace, purpose)
                },
                &mut |bytes| {
                    assert_eq!(phase, "entropy");
                    bytes.fill(7);
                    Err("private entropy diagnostic".into())
                },
            );
            match phase {
                "prompt" => assert_eq!(result.err().unwrap(), Error::PinInput),
                "entropy" => assert_eq!(result.err().unwrap(), Error::Entropy),
                _ => assert!(matches!(result, Err(Error::Protocol(_)))),
            }
            assert_eq!(
                trace.commands.get(),
                if phase == "capabilities" { 1 } else { 2 }
            );
            assert_eq!(trace.drops.get(), 1);
        }
    }

    #[test]
    fn signed_backup_flags_cannot_make_an_unlock_device_bound() {
        let label = "p2-scoped";
        for response in ["enroll_response", "enroll_be", "enroll_bs"] {
            let (mut channel, trace) = Script::new(label, false);
            channel.steps[3] = (fixture(label, "enroll_assertion"), fixture(label, response));
            let credential = fixture(label, "credential_id");
            let mut intent = assertion(label);
            intent.credential = &credential;
            let result = Transaction::new(channel).unwrap().assertion(
                intent,
                &mut |purpose| prompt(&trace, purpose),
                &mut entropy(label, false),
            );
            if response == "enroll_response" {
                assert_eq!(result.unwrap().bytes(), fixture(label, "output"));
            } else {
                assert_eq!(
                    result.err().unwrap(),
                    Error::Protocol("portable assertion is not device-bound".into())
                );
            }
            assert_eq!(trace.commands.get(), 4);
            assert_eq!(trace.drops.get(), 1);

            let (mut channel, trace) = Script::new(label, true);
            channel.steps[6].1 = fixture(label, response);
            let result = Transaction::new(channel).unwrap().enroll(
                enrollment(label),
                &mut |purpose| prompt(&trace, purpose),
                &mut entropy(label, true),
            );
            if response == "enroll_response" {
                assert_eq!(result.unwrap().output().bytes(), fixture(label, "output"));
            } else {
                assert_eq!(
                    result.err().unwrap(),
                    Error::Protocol("portable enrollment proof is not device-bound".into())
                );
            }
            assert_eq!(trace.commands.get(), 7);
            assert_eq!(trace.drops.get(), 1);
        }
    }

    const NO_CREDENTIALS: &[u8] = &[0x2e];
    const IDS: &[&[u8]] = &[b"login-key-a", b"login-key-b", b"login-key-c"];

    fn info_step(info: Info) -> (Vec<u8>, Vec<u8>) {
        (vec![4], info_with("p2-scoped", info))
    }
    fn listed(list: u8) -> (Vec<u8>, Vec<u8>) {
        info_step(Info {
            list: Some(list),
            ..Info::default()
        })
    }
    fn identify_step(request: &str, response: &[u8]) -> (Vec<u8>, Vec<u8>) {
        (
            login_fixture("identify", &format!("request_{request}")),
            response.to_vec(),
        )
    }
    fn select(name: &str) -> Vec<u8> {
        login_fixture("identify", name)
    }
    fn identify(
        steps: Vec<(Vec<u8>, Vec<u8>)>,
        credentials: &[&[u8]],
    ) -> (Result<Option<usize>, LoginError>, usize) {
        let (channel, trace) = Script::steps(steps);
        let hash = login_fixture("identify", "hash").try_into().unwrap();
        let result = Transaction::new(channel)
            .unwrap()
            .identify(credentials, hash);
        assert_eq!(trace.drops.get(), 1);
        (result, trace.commands.get())
    }

    #[test]
    fn identify_batches_to_the_advertised_limit_and_selects_by_index() {
        let absent = || info_step(Info::default());
        for (steps, expected) in [
            // Absent limit: one ID per batch, stopping at the selection.
            (
                vec![
                    absent(),
                    identify_step("a", NO_CREDENTIALS),
                    identify_step("b", &select("select_b")),
                ],
                Some(1),
            ),
            (
                vec![
                    listed(2),
                    identify_step("ab", NO_CREDENTIALS),
                    identify_step("c", &select("select_c")),
                ],
                Some(2),
            ),
            // A batch exactly at, or under, the advertised limit.
            (
                vec![listed(3), identify_step("abc", &select("select_b"))],
                Some(1),
            ),
            (
                vec![listed(23), identify_step("abc", &select("select_c"))],
                Some(2),
            ),
            // NO_CREDENTIALS from every batch is the only "not enrolled here".
            (
                vec![
                    absent(),
                    identify_step("a", NO_CREDENTIALS),
                    identify_step("b", NO_CREDENTIALS),
                    identify_step("c", NO_CREDENTIALS),
                ],
                None,
            ),
            (
                vec![
                    listed(2),
                    identify_step("ab", NO_CREDENTIALS),
                    identify_step("c", NO_CREDENTIALS),
                ],
                None,
            ),
        ] {
            let count = steps.len();
            let (result, commands) = identify(steps, IDS);
            assert_eq!(result, Ok(expected));
            assert_eq!(commands, count);
        }
        let (result, commands) = identify(
            vec![absent(), identify_step("a", &select("select_omitted"))],
            &IDS[..1],
        );
        assert_eq!((result, commands), (Ok(Some(0)), 2));
        // An ID longer than the key's advertised maximum is never sent.
        let short = || {
            info_step(Info {
                max_id: Some(11),
                ..Info::default()
            })
        };
        let long: &[&[u8]] = &[b"login-key-long", IDS[0]];
        let (result, commands) = identify(
            vec![short(), identify_step("a", &select("select_omitted"))],
            long,
        );
        assert_eq!((result, commands), (Ok(Some(1)), 2));
        let (result, commands) = identify(vec![short()], &long[..1]);
        assert_eq!((result, commands), (Ok(None), 1));
    }

    #[test]
    fn identify_fails_on_any_other_status_selection_or_capability() {
        for byte in [0x01, 0x22, 0x27, 0x2f, 0x31, 0x35, 0x3a, 0x7f] {
            let (result, commands) = identify(
                vec![
                    info_step(Info::default()),
                    identify_step("a", NO_CREDENTIALS),
                    identify_step("b", &[byte]),
                ],
                IDS,
            );
            assert_eq!(
                result,
                Err(LoginError::Failed(Error::Status(Status::decode(byte))))
            );
            assert_eq!(commands, 3);
        }
        for (steps, credentials) in [
            (
                vec![listed(2), identify_step("ab", &select("select_outside"))],
                IDS,
            ),
            (
                vec![listed(2), identify_step("ab", &select("select_omitted"))],
                IDS,
            ),
            (
                vec![listed(3), identify_step("abc", &select("select_up"))],
                IDS,
            ),
            (
                vec![listed(3), identify_step("abc", &select("select_uv"))],
                IDS,
            ),
            (vec![listed(2), identify_step("ab", &[0, 0xa0])], &IDS[..2]),
        ] {
            let (result, commands) = identify(steps, credentials);
            assert!(
                matches!(result, Err(LoginError::Failed(Error::Protocol(_)))),
                "{result:?}"
            );
            assert_eq!(commands, 2);
        }
        let (result, commands) = identify(vec![], &[]);
        assert!(matches!(
            result,
            Err(LoginError::Failed(Error::Protocol(_)))
        ));
        assert_eq!(commands, 0);
        let (result, commands) = identify(
            vec![info_step(Info {
                always_uv: Some(true),
                ..Info::default()
            })],
            IDS,
        );
        assert_eq!(result, Err(LoginError::Refused(LoginRefusal::AlwaysUv)));
        assert_eq!(commands, 1);
        let (mut channel, trace) = Script::steps(vec![
            info_step(Info::default()),
            identify_step("a", NO_CREDENTIALS),
        ]);
        channel.fail = Some(1);
        let hash = login_fixture("identify", "hash").try_into().unwrap();
        assert_eq!(
            Transaction::new(channel).unwrap().identify(IDS, hash),
            Err(LoginError::Failed(Error::Transport))
        );
        assert_eq!(trace.commands.get(), 2);
    }

    #[test]
    fn identify_requires_a_bare_no_credentials_answer() {
        let mut bodied = NO_CREDENTIALS.to_vec();
        bodied.push(0xa0);
        for (second, expected) in [
            (NO_CREDENTIALS.to_vec(), Ok(Some(2))),
            (bodied, Err(())),
            (vec![0x2e, 0], Err(())),
        ] {
            let (result, commands) = identify(
                vec![
                    info_step(Info::default()),
                    identify_step("a", NO_CREDENTIALS),
                    identify_step("b", &second),
                    identify_step("c", &select("select_c")),
                ],
                IDS,
            );
            match expected {
                Ok(index) => assert_eq!((result, commands), (Ok(index), 4)),
                Err(()) => {
                    assert!(
                        matches!(result, Err(LoginError::Failed(Error::Protocol(_)))),
                        "{result:?}"
                    );
                    assert_eq!(commands, 3);
                }
            }
        }
    }

    #[test]
    fn identify_packs_by_count_and_size_and_skips_what_cannot_fit() {
        let hash: [u8; 32] = login_fixture("identify", "hash").try_into().unwrap();
        let request = |ids: &[&[u8]]| {
            IdentifyRequest::new(ids, hash, 1024)
                .unwrap()
                .bytes()
                .to_vec()
        };
        // Eight foreign 120-byte IDs and the enrolled key's, on a key with the
        // default 1024-byte message limit and a list limit of eight.
        let foreign: Vec<Vec<u8>> = (0..8).map(|i| vec![i; 120]).collect();
        let mut ids: Vec<&[u8]> = foreign.iter().map(Vec::as_slice).collect();
        ids.push(IDS[2]);
        assert!(IdentifyRequest::new(&ids[..7], hash, 1024).is_err());
        assert_eq!(request(&ids[..6]).len(), 909);
        assert_eq!(request(&ids[6..]).len(), 373);
        let (result, commands) = identify(
            vec![
                listed(8),
                (request(&ids[..6]), NO_CREDENTIALS.to_vec()),
                (request(&ids[6..]), select("select_c")),
            ],
            &ids,
        );
        assert_eq!((result, commands), (Ok(Some(8)), 3));
        // Size closes a batch before the count does, and the count still binds.
        let (result, commands) = identify(
            vec![
                listed(4),
                (request(&ids[..4]), NO_CREDENTIALS.to_vec()),
                (request(&ids[4..8]), NO_CREDENTIALS.to_vec()),
                (request(&ids[8..]), select("select_omitted")),
            ],
            &ids,
        );
        assert_eq!((result, commands), (Ok(Some(8)), 4));
        // An ID whose own silent request exceeds the limit is never sent.
        let huge = vec![7; 1000];
        assert!(IdentifyRequest::new(&[&huge], hash, 1024).is_err());
        let (result, commands) = identify(
            vec![listed(8), identify_step("a", &select("select_omitted"))],
            &[&huge, IDS[0]],
        );
        assert_eq!((result, commands), (Ok(Some(1)), 2));
        let (result, commands) = identify(vec![listed(8)], &[&huge]);
        assert_eq!((result, commands), (Ok(None), 1));
        // A malformed list is the caller's error, before any token I/O.
        for credentials in [&[IDS[0], IDS[0]][..], &[IDS[0], b""], &[&[0; 1025]]] {
            let (result, commands) = identify(vec![], credentials);
            assert!(matches!(
                result,
                Err(LoginError::Failed(Error::Protocol(_)))
            ));
            assert_eq!(commands, 0);
        }
    }

    fn login_assertion_steps(label: &str, retries: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
        vec![
            (vec![4], fixture(label, "info")),
            (
                fixture(label, "key_request"),
                fixture(label, "key_response"),
            ),
            (
                login_fixture(label, "retries_request"),
                login_fixture("retries", retries),
            ),
            (
                fixture(label, "pin_request"),
                fixture(label, "pin_response"),
            ),
            (fixture(label, "assertion"), fixture(label, "response")),
        ]
    }
    type Prompted = Rc<RefCell<Vec<(PinPurpose, u8, usize)>>>;
    // Records each prompt with its retry count and how many commands preceded it.
    fn login_prompt(
        label: &str,
        trace: &Rc<Trace>,
    ) -> (
        impl FnMut(PinPurpose, u8) -> Result<Pin, String> + use<>,
        Prompted,
    ) {
        let seen = Prompted::default();
        let (record, trace, pin) = (seen.clone(), trace.clone(), fixture(label, "pin"));
        let prompt = move |purpose, left| {
            record
                .borrow_mut()
                .push((purpose, left, trace.commands.get()));
            Pin::new(pin.clone().into_boxed_slice())
        };
        (prompt, seen)
    }
    type Hashed = Result<(Pin, [u8; 32]), String>;
    /// `login_prompt` returning the fixture's hash with the PIN.
    fn assertion_prompt(
        label: &str,
        trace: &Rc<Trace>,
    ) -> (impl FnMut(PinPurpose, u8) -> Hashed + use<>, Prompted) {
        let (mut prompt, seen) = login_prompt(label, trace);
        let (_, challenge) = login_intent(label);
        (
            move |purpose, left| Ok((prompt(purpose, left)?, challenge)),
            seen,
        )
    }

    #[test]
    fn login_assertion_queries_retries_immediately_before_its_one_prompt() {
        for label in LABELS {
            for (retries, count) in [("eight", 8), ("five", 5)] {
                let (channel, trace) = Script::steps(login_assertion_steps(label, retries));
                let (mut prompt, seen) = assertion_prompt(label, &trace);
                let output = Transaction::new(channel)
                    .unwrap()
                    .login_assertion(
                        login_intent(label).0,
                        &mut prompt,
                        &mut entropy(label, false),
                    )
                    .unwrap();
                assert_eq!(output.bytes(), fixture(label, "output"));
                assert!(output.info.user_verified);
                assert_eq!(*seen.borrow(), [(PinPurpose::Assertion, count, 3)]);
                assert_eq!(trace.commands.get(), 5);
                assert_eq!(trace.drops.get(), 1);
            }
        }
    }

    #[test]
    fn exhausted_retries_get_no_prompt_and_pin_statuses_stay_typed() {
        let label = "p2-scoped";
        let run = |steps: Vec<(Vec<u8>, Vec<u8>)>| {
            let (channel, trace) = Script::steps(steps);
            let (mut prompt, seen) = assertion_prompt(label, &trace);
            let result = Transaction::new(channel).unwrap().login_assertion(
                login_intent(label).0,
                &mut prompt,
                &mut entropy(label, false),
            );
            assert_eq!(trace.drops.get(), 1);
            let prompts = seen.borrow().len();
            (result.err(), trace.commands.get(), prompts)
        };
        let failed = |status| Some(LoginError::Failed(Error::Status(status)));
        for (retries, status) in [
            ("zero", Status::PinBlocked),
            ("power", Status::PinAuthBlocked),
        ] {
            let mut steps = login_assertion_steps(label, retries);
            steps.truncate(3);
            assert_eq!(run(steps), (failed(status), 3, 0));
        }
        // No retries left is PIN blocked, whatever the power-cycle state.
        let mut steps = login_assertion_steps(label, "zero");
        steps[2].1 = b"\x00\xa2\x03\x00\x04\xf5".to_vec();
        steps.truncate(3);
        assert_eq!(run(steps), (failed(Status::PinBlocked), 3, 0));
        for (byte, status) in [
            (0x31, Status::PinInvalid),
            (0x32, Status::PinBlocked),
            (0x33, Status::PinAuthInvalid),
            (0x34, Status::PinAuthBlocked),
        ] {
            let mut steps = login_assertion_steps(label, "eight");
            steps[2].1 = vec![byte];
            steps.truncate(3);
            assert_eq!(run(steps), (failed(status), 3, 0));
            let mut steps = login_assertion_steps(label, "eight");
            steps[3].1 = vec![byte];
            steps.truncate(4);
            assert_eq!(run(steps), (failed(status), 4, 1));
        }
        let mut steps = login_assertion_steps(label, "eight");
        steps[2].1 = vec![0, 0xa0];
        steps.truncate(3);
        let (error, commands, prompts) = run(steps);
        assert!(matches!(
            error,
            Some(LoginError::Failed(Error::Protocol(_)))
        ));
        assert_eq!((commands, prompts), (3, 0));
        let (channel, _) = Script::steps(login_assertion_steps(label, "eight"));
        assert_eq!(
            Transaction::new(channel)
                .unwrap()
                .login_assertion(
                    login_intent(label).0,
                    &mut |_, _| Err("prompt cancelled".into()),
                    &mut |_| panic!("unexpected entropy"),
                )
                .err(),
            Some(LoginError::Failed(Error::PinInput))
        );
        // Signed backup flags never unlock.
        let credential = fixture(label, "credential_id");
        for response in ["enroll_be", "enroll_bs"] {
            let mut steps = login_assertion_steps(label, "eight");
            steps[4] = (fixture(label, "enroll_assertion"), fixture(label, response));
            let (channel, trace) = Script::steps(steps);
            let (mut prompt, _) = assertion_prompt(label, &trace);
            let (mut intent, _) = login_intent(label);
            intent.credential = &credential;
            assert_eq!(
                Transaction::new(channel)
                    .unwrap()
                    .login_assertion(intent, &mut prompt, &mut entropy(label, false))
                    .err(),
                Some(LoginError::Failed(Error::Protocol(
                    "login assertion is not device-bound".into()
                )))
            );
        }
    }

    fn login_create_steps(label: &str, info: Vec<u8>, make: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
        vec![
            (vec![4], info),
            (
                fixture(label, "key_request"),
                fixture(label, "create_key_response"),
            ),
            (
                login_fixture(label, "retries_request"),
                login_fixture("retries", "eight"),
            ),
            (
                fixture(label, "create_pin_request"),
                fixture(label, "create_pin_response"),
            ),
            (login_fixture(label, make), fixture(label, "make_none")),
            (
                fixture(label, "key_request"),
                fixture(label, "key_response"),
            ),
            (
                login_fixture(label, "retries_request"),
                login_fixture("retries", "five"),
            ),
            (
                fixture(label, "pin_request"),
                fixture(label, "pin_response"),
            ),
            (
                fixture(label, "enroll_assertion"),
                fixture(label, "enroll_response"),
            ),
        ]
    }

    #[test]
    fn login_create_uses_login_labels_and_retries_before_each_prompt() {
        let prior: &[&[u8]] = &[b"prior-primary", b"prior-backup"];
        for label in LABELS {
            for (list, excluded, make) in [
                (None, &[][..], "login_make_request"),
                (Some(2), prior, "login_make_excluded"),
            ] {
                let info = info_with(
                    label,
                    Info {
                        list,
                        always_uv: Some(false),
                        ..Info::default()
                    },
                );
                let (channel, trace) = Script::steps(login_create_steps(label, info, make));
                let (mut prompt, seen) = login_prompt(label, &trace);
                let mut intent = enrollment(label);
                intent.excluded = excluded;
                let output = Transaction::new(channel)
                    .unwrap()
                    .login_create(intent, &mut prompt, &mut entropy(label, true))
                    .unwrap();
                assert_eq!(output.id(), fixture(label, "credential_id"));
                assert_eq!(output.cose(), fixture(label, "cose"));
                assert_eq!(output.output().bytes(), fixture(label, "output"));
                assert_eq!(
                    *seen.borrow(),
                    [
                        (PinPurpose::Creation, 8, 3),
                        (PinPurpose::EnrollmentProof, 5, 7)
                    ]
                );
                assert_eq!(trace.commands.get(), 9);
                assert_eq!(trace.drops.get(), 1);
            }
        }
        // A proof whose key reports no retries left stops before its prompt.
        let label = "p2-scoped";
        let mut steps = login_create_steps(label, fixture(label, "info"), "login_make_request");
        steps[6].1 = login_fixture("retries", "zero");
        steps.truncate(7);
        let (channel, trace) = Script::steps(steps);
        let (mut prompt, seen) = login_prompt(label, &trace);
        assert_eq!(
            Transaction::new(channel)
                .unwrap()
                .login_create(enrollment(label), &mut prompt, &mut entropy(label, true))
                .err(),
            Some(LoginError::Failed(Error::Status(Status::PinBlocked)))
        );
        assert_eq!(seen.borrow().len(), 1);
        assert_eq!(trace.commands.get(), 7);
    }

    #[test]
    fn each_login_refusal_stops_after_get_info_without_a_prompt() {
        let label = "p2-scoped";
        let two: &[&[u8]] = &[b"prior-primary", b"prior-backup"];
        let refusals =
            [Extensions::Other, Extensions::Empty, Extensions::Absent].map(|extensions| {
                (
                    Info {
                        extensions,
                        ..Info::default()
                    },
                    LoginRefusal::NoHmacSecret,
                    &[][..],
                )
            });
        for (info, refusal, excluded) in refusals.into_iter().chain([
            (
                Info {
                    always_uv: Some(true),
                    ..Info::default()
                },
                LoginRefusal::AlwaysUv,
                &[][..],
            ),
            (
                Info {
                    client_pin: Some(false),
                    ..Info::default()
                },
                LoginRefusal::PinNotSet,
                &[],
            ),
            (
                Info {
                    client_pin: None,
                    ..Info::default()
                },
                LoginRefusal::PinUnsupported,
                &[],
            ),
            (
                Info {
                    options: false,
                    ..Info::default()
                },
                LoginRefusal::PinUnsupported,
                &[],
            ),
            (Info::default(), LoginRefusal::ListTooSmall, two),
            (
                Info {
                    list: Some(1),
                    ..Info::default()
                },
                LoginRefusal::ListTooSmall,
                two,
            ),
        ]) {
            let info = info_with(label, info);
            let (channel, trace) = Script::steps(vec![(vec![4], info.clone())]);
            let mut intent = enrollment(label);
            intent.excluded = excluded;
            let refused = Some(LoginError::Refused(refusal));
            assert_eq!(
                Transaction::new(channel)
                    .unwrap()
                    .login_create(
                        intent,
                        &mut |_, _| panic!("unexpected prompt"),
                        &mut |_| panic!("unexpected entropy")
                    )
                    .err(),
                refused
            );
            assert_eq!(trace.commands.get(), 1);
            assert_eq!(trace.drops.get(), 1);
            if refusal == LoginRefusal::ListTooSmall {
                continue;
            }
            let (channel, trace) = Script::steps(vec![(vec![4], info)]);
            assert_eq!(
                Transaction::new(channel)
                    .unwrap()
                    .login_assertion(
                        login_intent(label).0,
                        &mut |_, _| panic!("unexpected prompt"),
                        &mut |_| panic!("unexpected entropy")
                    )
                    .err(),
                refused
            );
            assert_eq!(trace.commands.get(), 1);
        }
        let (channel, trace) = Script::steps(vec![]);
        let mut intent = enrollment(label);
        intent.proof_challenge = intent.challenge;
        assert!(matches!(
            Transaction::new(channel).unwrap().login_create(
                intent,
                &mut |_, _| panic!("unexpected prompt"),
                &mut |_| panic!("unexpected entropy")
            ),
            Err(LoginError::Failed(Error::Protocol(_)))
        ));
        assert_eq!(trace.commands.get(), 0);
    }
}
