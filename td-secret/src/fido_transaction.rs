//! Owned portable PIN transactions. Transport admission and presentation stay backend duties.

use super::fido_device::{Interruption, Session};
use super::fido_hid::Message;
use super::fido_p256::PublicKey;
use super::fido_pin::{EnrolledCredential, HmacOutput, Pin, Profile};

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
pub(super) struct Enrollment<'a> {
    pub challenge: [u8; 32],
    pub user: [u8; 32],
    pub proof_challenge: [u8; 32],
    pub salt: [u8; 32],
    pub excluded: &'a [&'a [u8]],
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
        self.check()?;
        let reply = self.channel.exchange(bytes);
        let active = self.channel.check();
        if let Err(reason @ (Interruption::Cancelled | Interruption::Expired)) = active {
            return Err(Error::Interrupted(reason));
        }
        let reply = reply.map_err(|_| Error::Transport)?;
        active.map_err(Error::Interrupted)?;
        let status = reply
            .as_ref()
            .first()
            .copied()
            .ok_or_else(|| Error::Protocol("missing portable CTAP status".into()))?;
        if status != 0 {
            return Err(Error::Status(Status::decode(status)));
        }
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
        let reply = self.command(request.bytes())?;
        let pin = self.collect_pin(PinPurpose::Assertion, prompt)?;
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
        let output = self.transition(request.finish(reply.as_ref()))?;
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
        let reply = self.command(request.bytes())?;
        let pin = self.collect_pin(PinPurpose::Creation, prompt)?;
        let request = self.with_entropy(entropy, |entropy| {
            request.with_pin(reply.as_ref(), pin, &mut |bytes| entropy(bytes))
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
        let pin = self.collect_pin(PinPurpose::EnrollmentProof, prompt)?;
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
        let output = self.transition(request.finish(reply.as_ref()))?;
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

    fn fixture(label: &str, name: &str) -> Vec<u8> {
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
    fn assertion(label: &str) -> Assertion<'static> {
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
    fn enrollment(label: &str) -> Enrollment<'static> {
        Enrollment {
            challenge: fixture(label, "create_challenge").try_into().unwrap(),
            user: fixture(label, "user").try_into().unwrap(),
            proof_challenge: fixture(label, "challenge").try_into().unwrap(),
            salt: fixture(label, "salt").try_into().unwrap(),
            excluded: &[],
        }
    }
    fn entropy(label: &str, creation: bool) -> impl FnMut(&mut [u8]) -> Result<(), String> + '_ {
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
    fn message(bytes: &[u8]) -> Message {
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
    struct Trace {
        commands: Cell<usize>,
        drops: Cell<usize>,
        interrupted: Cell<Option<Interruption>>,
        final_checks: Cell<usize>,
        pins: RefCell<Vec<PinPurpose>>,
    }
    struct Script {
        trace: Rc<Trace>,
        steps: VecDeque<(Vec<u8>, Vec<u8>)>,
        fail: Option<usize>,
        late: bool,
    }
    impl Script {
        fn new(label: &str, creation: bool) -> (Self, Rc<Trace>) {
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
}
