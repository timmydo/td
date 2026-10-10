//! The committed CTAP transcripts as fixtures: their vector rows, a scripted
//! channel replaying them, and the re-executed worker serving them over the
//! hidraw worker protocol. Test-only; td-secret's tests compile it by path.

use crate::fido_device::Interruption;
use crate::fido_hid::{self as hid, Event, Message};
use crate::fido_p256::PublicKey;
use crate::fido_transaction::{
    Assertion, Channel, Enrollment, LoginAssertion, LoginCreation, PinPurpose,
};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::rc::Rc;

pub(crate) const LABELS: &[&str] = &["p1-legacy", "p1-scoped", "p2-legacy", "p2-scoped"];

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
/// The fixture creation as a login intent; `creation_prompt` returns its hashes.
pub(crate) fn login_creation(label: &str) -> LoginCreation<'static> {
    let Enrollment {
        user,
        salt,
        excluded,
        ..
    } = enrollment(label);
    LoginCreation {
        user,
        salt,
        excluded,
    }
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
    pub(crate) interrupted: Cell<Option<Interruption>>,
    final_checks: Cell<usize>,
    pub(crate) pins: RefCell<Vec<PinPurpose>>,
}
pub(crate) struct Script {
    trace: Rc<Trace>,
    pub(crate) steps: VecDeque<(Vec<u8>, Vec<u8>)>,
    pub(crate) fail: Option<usize>,
    pub(crate) late: bool,
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

    pub(crate) fn steps(steps: Vec<(Vec<u8>, Vec<u8>)>) -> (Self, Rc<Trace>) {
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
