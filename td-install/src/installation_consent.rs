//! The private channel between td-authd and the installation service it
//! supervises (INSTALLER.md "Installation consent channel"). The service
//! reports one review's display facts and its fate; td-authd answers with
//! the physical consent decision. Both ends are root and td-authd created
//! the channel, so decoding authenticates nothing: it only refuses bytes
//! outside the grammar. This file is self-contained because td-authd
//! compiles it too.

/// Sent and required by both ends before the first frame. A change to any
/// message or its bytes changes the greeting; there is no negotiation.
pub const GREETING: &[u8; 8] = b"TDINA02\n";
pub const MAX_MESSAGE_BYTES: usize = 1024;

const NONCE_BYTES: usize = 32;
/// The kernel's DISK_NAME_LEN less its NUL; consent shows no wider name.
const DISK_BYTES: usize = 31;
const LABEL_BYTES: usize = 256;
const HOSTNAME_BYTES: usize = 63;
const USERNAME_BYTES: usize = 32;
/// The widest message either end sends.
const REVIEW_BYTES: usize = 1
    + NONCE_BYTES
    + (1 + DISK_BYTES)
    + 8
    + 2 * (1 + 2 + LABEL_BYTES)
    + (1 + HOSTNAME_BYTES)
    + (1 + USERNAME_BYTES)
    + 32
    + 1;
const _: () = assert!(REVIEW_BYTES <= MAX_MESSAGE_BYTES);

const REVIEW: u8 = 0x01;
const ENDED: u8 = 0x02;
const STARTED: u8 = 0x03;
const FINISHED: u8 = 0x04;
// td-authd's answers set the high bit, so a reflected frame never decodes.
const CONSENT: u8 = 0x81;
const DECLINED: u8 = 0x82;

pub fn check_greeting(received: &[u8; 8]) -> Result<(), String> {
    if received == GREETING {
        Ok(())
    } else {
        Err("unsupported installation consent greeting".into())
    }
}

/// One frame: a big-endian u32 length, then the payload.
pub fn frame(payload: &[u8]) -> Result<Vec<u8>, String> {
    if payload.is_empty() || payload.len() > MAX_MESSAGE_BYTES {
        return Err("installation consent frame outside its bound".into());
    }
    let length =
        u32::try_from(payload.len()).map_err(|_| "installation consent frame outside its bound")?;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// The admitted payload length, checked before the caller allocates for it.
pub fn payload_len(header: [u8; 4]) -> Result<usize, String> {
    match usize::try_from(u32::from_be_bytes(header)) {
        Ok(length) if length != 0 && length <= MAX_MESSAGE_BYTES => Ok(length),
        _ => Err("installation consent frame outside its bound".into()),
    }
}

/// The held review's storage, as the plan's storage byte carries it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Storage {
    Unencrypted,
    DeviceBound,
}

impl Storage {
    pub const ALL: &[Self] = &[Self::Unencrypted, Self::DeviceBound];

    fn code(self) -> u8 {
        match self {
            Self::Unencrypted => 0,
            Self::DeviceBound => 1,
        }
    }

    // A repeated code would silently shadow a variant.
    #[deny(unreachable_patterns)]
    fn from_code(code: u8) -> Result<Self, String> {
        match code {
            0 => Ok(Self::Unencrypted),
            1 => Ok(Self::DeviceBound),
            _ => Err("unknown installation consent storage".into()),
        }
    }
}

/// What a consent prompt shows of one review. The service fills it from the
/// review it holds; td-authd escapes, narrows and lays it out, and refuses
/// what it cannot show.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Review {
    nonce: [u8; NONCE_BYTES],
    disk: String,
    capacity: u64,
    model: Option<String>,
    serial: Option<String>,
    hostname: String,
    username: String,
    deployment: [u8; 32],
    storage: Storage,
}

/// Unvalidated review fields. Construction copies admitted values.
#[derive(Debug)]
pub struct ReviewFields<'a> {
    pub nonce: [u8; NONCE_BYTES],
    pub disk: &'a str,
    pub capacity: u64,
    pub model: Option<&'a str>,
    pub serial: Option<&'a str>,
    pub hostname: &'a str,
    pub username: &'a str,
    pub deployment: [u8; 32],
    pub storage: Storage,
}

impl Review {
    pub fn new(fields: ReviewFields<'_>) -> Result<Self, String> {
        let ReviewFields {
            nonce,
            disk,
            capacity,
            model,
            serial,
            hostname,
            username,
            deployment,
            storage,
        } = fields;
        check_nonce(&nonce)?;
        if disk.is_empty()
            || disk.len() > DISK_BYTES
            || !disk
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Err("invalid installation consent disk name".into());
        }
        if capacity == 0 {
            return Err("installation consent capacity cannot be zero".into());
        }
        if [model, serial]
            .into_iter()
            .flatten()
            .any(|label| label.len() > LABEL_BYTES)
        {
            return Err("installation consent label exceeds byte bound".into());
        }
        if hostname.is_empty() || hostname.len() > HOSTNAME_BYTES {
            return Err("invalid installation consent hostname".into());
        }
        if username.is_empty() || username.len() > USERNAME_BYTES {
            return Err("invalid installation consent username".into());
        }
        Ok(Self {
            nonce,
            disk: disk.into(),
            capacity,
            model: model.map(str::to_owned),
            serial: serial.map(str::to_owned),
            hostname: hostname.into(),
            username: username.into(),
            deployment,
            storage,
        })
    }
    pub fn nonce(&self) -> &[u8; NONCE_BYTES] {
        &self.nonce
    }
    pub fn disk(&self) -> &str {
        &self.disk
    }
    pub fn capacity(&self) -> u64 {
        self.capacity
    }
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
    pub fn serial(&self) -> Option<&str> {
        self.serial.as_deref()
    }
    pub fn hostname(&self) -> &str {
        &self.hostname
    }
    pub fn username(&self) -> &str {
        &self.username
    }
    pub fn deployment(&self) -> &[u8; 32] {
        &self.deployment
    }
    pub fn storage(&self) -> Storage {
        self.storage
    }
}

/// Why a review ended before any destructive write, as the service saw it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ended {
    /// The installer withdrew it.
    Withdrawn,
    /// The held disk changed, vanished or failed a recheck.
    DestinationChanged,
    /// The installer's channel closed before the installation started.
    InstallerLost,
    /// td-authd answered without consent.
    NotConsented,
}

impl Ended {
    pub const ALL: &[Self] = &[
        Self::Withdrawn,
        Self::DestinationChanged,
        Self::InstallerLost,
        Self::NotConsented,
    ];

    fn code(self) -> u8 {
        match self {
            Self::Withdrawn => 1,
            Self::DestinationChanged => 2,
            Self::InstallerLost => 3,
            Self::NotConsented => 4,
        }
    }

    // A repeated code would silently shadow a variant.
    #[deny(unreachable_patterns)]
    fn from_code(code: u8) -> Result<Self, String> {
        match code {
            1 => Ok(Self::Withdrawn),
            2 => Ok(Self::DestinationChanged),
            3 => Ok(Self::InstallerLost),
            4 => Ok(Self::NotConsented),
            _ => Err("unknown installation consent ending".into()),
        }
    }
}

/// How a started installation finished.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Complete,
    /// The disk may be incomplete.
    Failed,
}

impl Outcome {
    pub const ALL: &[Self] = &[Self::Complete, Self::Failed];

    fn code(self) -> u8 {
        match self {
            Self::Complete => 1,
            Self::Failed => 2,
        }
    }

    // A repeated code would silently shadow a variant.
    #[deny(unreachable_patterns)]
    fn from_code(code: u8) -> Result<Self, String> {
        match code {
            1 => Ok(Self::Complete),
            2 => Ok(Self::Failed),
            _ => Err("unknown installation outcome".into()),
        }
    }
}

/// Why td-authd answered without consent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoConsent {
    /// The person cancelled the prompt.
    Declined,
    /// The prompt timed out.
    Expired,
    /// No trusted path could show the prompt, td-authd cannot show these
    /// facts, or the prompt was lost while shown.
    Unavailable,
}

impl NoConsent {
    pub const ALL: &[Self] = &[Self::Declined, Self::Expired, Self::Unavailable];

    fn code(self) -> u8 {
        match self {
            Self::Declined => 1,
            Self::Expired => 2,
            Self::Unavailable => 3,
        }
    }

    // A repeated code would silently shadow a variant.
    #[deny(unreachable_patterns)]
    fn from_code(code: u8) -> Result<Self, String> {
        match code {
            1 => Ok(Self::Declined),
            2 => Ok(Self::Expired),
            3 => Ok(Self::Unavailable),
            _ => Err("unknown installation consent refusal".into()),
        }
    }
}

/// What the service sends td-authd. Every report after a review names that
/// review's nonce.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Report {
    /// Ask for consent to this review.
    Review(Box<Review>),
    /// The review ended before any destructive write.
    Ended([u8; NONCE_BYTES], Ended),
    /// Consent was received and the final recheck passed; the first write
    /// follows.
    Started([u8; NONCE_BYTES]),
    Finished([u8; NONCE_BYTES], Outcome),
}

impl Report {
    pub fn nonce(&self) -> &[u8; NONCE_BYTES] {
        match self {
            Self::Review(review) => review.nonce(),
            Self::Ended(nonce, _) | Self::Started(nonce) | Self::Finished(nonce, _) => nonce,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(REVIEW_BYTES);
        match self {
            Self::Review(review) => {
                out.push(REVIEW);
                out.extend_from_slice(&review.nonce);
                put_text(&mut out, &review.disk);
                out.extend_from_slice(&review.capacity.to_be_bytes());
                put_label(&mut out, review.model.as_deref());
                put_label(&mut out, review.serial.as_deref());
                put_text(&mut out, &review.hostname);
                put_text(&mut out, &review.username);
                out.extend_from_slice(&review.deployment);
                out.push(review.storage.code());
            }
            Self::Ended(nonce, why) => {
                out.push(ENDED);
                out.extend_from_slice(nonce);
                out.push(why.code());
            }
            Self::Started(nonce) => {
                out.push(STARTED);
                out.extend_from_slice(nonce);
            }
            Self::Finished(nonce, outcome) => {
                out.push(FINISHED);
                out.extend_from_slice(nonce);
                out.push(outcome.code());
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut reader = Reader(bytes);
        let report = match reader.byte()? {
            REVIEW => {
                let nonce = reader.array()?;
                let disk = reader.text()?;
                let capacity = u64::from_be_bytes(reader.array()?);
                let model = reader.label()?;
                let serial = reader.label()?;
                let hostname = reader.text()?;
                let username = reader.text()?;
                let deployment = reader.array()?;
                let storage = Storage::from_code(reader.byte()?)?;
                Self::Review(Box::new(Review::new(ReviewFields {
                    nonce,
                    disk,
                    capacity,
                    model,
                    serial,
                    hostname,
                    username,
                    deployment,
                    storage,
                })?))
            }
            ENDED => {
                let nonce = reader.nonce()?;
                Self::Ended(nonce, Ended::from_code(reader.byte()?)?)
            }
            STARTED => Self::Started(reader.nonce()?),
            FINISHED => {
                let nonce = reader.nonce()?;
                Self::Finished(nonce, Outcome::from_code(reader.byte()?)?)
            }
            _ => return Err("unknown installation consent report".into()),
        };
        reader.end()?;
        Ok(report)
    }
}

/// What td-authd sends the service: one answer to each review, naming it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Answer {
    /// The person confirmed this review on the trusted path.
    Consent([u8; NONCE_BYTES]),
    Declined([u8; NONCE_BYTES], NoConsent),
}

impl Answer {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + NONCE_BYTES);
        match self {
            Self::Consent(nonce) => {
                out.push(CONSENT);
                out.extend_from_slice(nonce);
            }
            Self::Declined(nonce, why) => {
                out.push(DECLINED);
                out.extend_from_slice(nonce);
                out.push(why.code());
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut reader = Reader(bytes);
        let answer = match reader.byte()? {
            CONSENT => Self::Consent(reader.nonce()?),
            DECLINED => {
                let nonce = reader.nonce()?;
                Self::Declined(nonce, NoConsent::from_code(reader.byte()?)?)
            }
            _ => return Err("unknown installation consent answer".into()),
        };
        reader.end()?;
        Ok(answer)
    }

    pub fn nonce(&self) -> &[u8; NONCE_BYTES] {
        match self {
            Self::Consent(nonce) | Self::Declined(nonce, _) => nonce,
        }
    }
}

fn check_nonce(nonce: &[u8; NONCE_BYTES]) -> Result<(), String> {
    if *nonce == [0; NONCE_BYTES] {
        return Err("installation consent nonce cannot be zero".into());
    }
    Ok(())
}

/// A one-byte length, then the bytes. Every text here is at most 63 bytes.
fn put_text(out: &mut Vec<u8>, text: &str) {
    out.push(text.len() as u8);
    out.extend_from_slice(text.as_bytes());
}

/// 0 for absent, or 1 and a big-endian u16 length, then the bytes.
fn put_label(out: &mut Vec<u8>, label: Option<&str>) {
    match label {
        None => out.push(0),
        Some(label) => {
            out.push(1);
            out.extend_from_slice(&(label.len() as u16).to_be_bytes());
            out.extend_from_slice(label.as_bytes());
        }
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let Some((taken, rest)) = self.0.split_at_checked(count) else {
            return Err("truncated installation consent message".into());
        };
        self.0 = rest;
        Ok(taken)
    }
    fn byte(&mut self) -> Result<u8, String> {
        let [byte] = self.array()?;
        Ok(byte)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        self.take(N)?
            .try_into()
            .map_err(|_| "truncated installation consent message".into())
    }
    fn nonce(&mut self) -> Result<[u8; NONCE_BYTES], String> {
        let nonce = self.array()?;
        check_nonce(&nonce)?;
        Ok(nonce)
    }
    fn utf8(&mut self, count: usize) -> Result<&'a str, String> {
        std::str::from_utf8(self.take(count)?)
            .map_err(|_| "installation consent text is not UTF-8".into())
    }
    fn text(&mut self) -> Result<&'a str, String> {
        let count = self.byte()?;
        self.utf8(usize::from(count))
    }
    fn label(&mut self) -> Result<Option<&'a str>, String> {
        match self.byte()? {
            0 => Ok(None),
            1 => {
                let count = u16::from_be_bytes(self.array()?);
                self.utf8(usize::from(count)).map(Some)
            }
            _ => Err("invalid installation consent label presence".into()),
        }
    }
    fn end(&self) -> Result<(), String> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err("trailing bytes after installation consent message".into())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn widest() -> Review {
        Review::new(ReviewFields {
            nonce: [7; 32],
            disk: &"n".repeat(DISK_BYTES),
            capacity: u64::MAX,
            model: Some(&"m".repeat(LABEL_BYTES)),
            serial: Some(&"s".repeat(LABEL_BYTES)),
            hostname: &"h".repeat(HOSTNAME_BYTES),
            username: &"u".repeat(USERNAME_BYTES),
            deployment: [9; 32],
            storage: Storage::DeviceBound,
        })
        .unwrap()
    }

    fn reports() -> Vec<Report> {
        let mut all = vec![
            Report::Review(Box::new(widest())),
            Report::Review(Box::new(
                Review::new(ReviewFields {
                    nonce: [1; 32],
                    disk: "vda",
                    capacity: 8 << 30,
                    model: None,
                    serial: Some(""),
                    hostname: "td",
                    username: "alice",
                    deployment: [0; 32],
                    storage: Storage::Unencrypted,
                })
                .unwrap(),
            )),
            Report::Started([3; 32]),
        ];
        all.extend(Ended::ALL.iter().map(|why| Report::Ended([2; 32], *why)));
        all.extend(
            Outcome::ALL
                .iter()
                .map(|outcome| Report::Finished([4; 32], *outcome)),
        );
        all
    }

    fn answers() -> Vec<Answer> {
        let mut all = vec![Answer::Consent([5; 32])];
        all.extend(
            NoConsent::ALL
                .iter()
                .map(|why| Answer::Declined([6; 32], *why)),
        );
        all
    }

    #[test]
    fn every_message_round_trips_within_one_frame() {
        let mut widest_bytes = 0;
        for report in reports() {
            let bytes = report.encode();
            widest_bytes = widest_bytes.max(bytes.len());
            let decoded = Report::decode(&bytes).unwrap();
            assert_eq!(decoded.nonce(), report.nonce());
            assert_eq!(decoded, report);
            assert!(Answer::decode(&bytes).is_err(), "{report:?}");
            let framed = frame(&bytes).unwrap();
            assert_eq!(
                payload_len(framed[..4].try_into().unwrap()).unwrap(),
                bytes.len()
            );
        }
        assert_eq!(widest_bytes, REVIEW_BYTES);
        assert_eq!(REVIEW_BYTES, 721);
        for answer in answers() {
            let bytes = answer.encode();
            assert_eq!(Answer::decode(&bytes).unwrap(), answer);
            assert!(Report::decode(&bytes).is_err(), "{answer:?}");
        }
    }

    #[test]
    fn frames_outside_the_bound_are_refused() {
        assert!(frame(&[]).is_err());
        assert!(frame(&[0; MAX_MESSAGE_BYTES + 1]).is_err());
        assert!(frame(&[0; MAX_MESSAGE_BYTES]).is_ok());
        assert!(payload_len([0; 4]).is_err());
        assert!(payload_len((MAX_MESSAGE_BYTES as u32 + 1).to_be_bytes()).is_err());
        assert_eq!(
            payload_len((MAX_MESSAGE_BYTES as u32).to_be_bytes()).unwrap(),
            MAX_MESSAGE_BYTES
        );
        assert!(check_greeting(GREETING).is_ok());
        assert!(check_greeting(b"TDINS01\n").is_err());
        // The version before the storage byte.
        assert!(check_greeting(b"TDINA01\n").is_err());
    }

    #[test]
    fn every_truncation_and_extension_is_refused() {
        for report in reports() {
            let bytes = report.encode();
            for cut in 0..bytes.len() {
                assert!(Report::decode(&bytes[..cut]).is_err(), "{report:?} {cut}");
            }
            let mut longer = bytes.clone();
            longer.push(0);
            assert!(Report::decode(&longer).is_err(), "{report:?}");
        }
        for answer in answers() {
            let bytes = answer.encode();
            for cut in 0..bytes.len() {
                assert!(Answer::decode(&bytes[..cut]).is_err(), "{answer:?} {cut}");
            }
            let mut longer = bytes.clone();
            longer.push(0);
            assert!(Answer::decode(&longer).is_err(), "{answer:?}");
        }
    }

    #[test]
    fn the_bytes_are_the_documented_ones() {
        let nonce = |byte| [byte; 32];
        let golden = |parts: &[&[u8]]| parts.concat();
        let review = Review::new(ReviewFields {
            nonce: nonce(1),
            disk: "vda",
            capacity: 8 << 30,
            model: Some("M"),
            serial: None,
            hostname: "td",
            username: "alice",
            deployment: [0xab; 32],
            storage: Storage::Unencrypted,
        })
        .unwrap();
        let bound = Review::new(ReviewFields {
            nonce: nonce(1),
            disk: "vda",
            capacity: 8 << 30,
            model: None,
            serial: None,
            hostname: "td",
            username: "alice",
            deployment: [0xab; 32],
            storage: Storage::DeviceBound,
        })
        .unwrap();
        for (message, bytes) in [
            (
                Report::Review(Box::new(bound)).encode(),
                golden(&[
                    &[0x01],
                    &nonce(1),
                    &[3],
                    b"vda",
                    &[0, 0, 0, 2, 0, 0, 0, 0],
                    &[0],
                    &[0],
                    &[2],
                    b"td",
                    &[5],
                    b"alice",
                    &[0xab; 32],
                    &[1],
                ]),
            ),
            (
                Report::Review(Box::new(review)).encode(),
                golden(&[
                    &[0x01],
                    &nonce(1),
                    &[3],
                    b"vda",
                    &[0, 0, 0, 2, 0, 0, 0, 0],
                    &[1, 0, 1],
                    b"M",
                    &[0],
                    &[2],
                    b"td",
                    &[5],
                    b"alice",
                    &[0xab; 32],
                    &[0],
                ]),
            ),
            (
                Report::Ended(nonce(2), Ended::NotConsented).encode(),
                golden(&[&[0x02], &nonce(2), &[4]]),
            ),
            (
                Report::Started(nonce(3)).encode(),
                golden(&[&[0x03], &nonce(3)]),
            ),
            (
                Report::Finished(nonce(4), Outcome::Failed).encode(),
                golden(&[&[0x04], &nonce(4), &[2]]),
            ),
            (
                Answer::Consent(nonce(5)).encode(),
                golden(&[&[0x81], &nonce(5)]),
            ),
            (
                Answer::Declined(nonce(6), NoConsent::Expired).encode(),
                golden(&[&[0x82], &nonce(6), &[2]]),
            ),
        ] {
            assert_eq!(message, bytes);
        }
    }

    #[test]
    fn codes_are_dense_and_nothing_else_decodes() {
        fn accepted<T>(from_code: fn(u8) -> Result<T, String>) -> Vec<u8> {
            (0..=u8::MAX)
                .filter(|code| from_code(*code).is_ok())
                .collect()
        }
        assert_eq!(accepted(Ended::from_code), [1, 2, 3, 4]);
        assert_eq!(accepted(Outcome::from_code), [1, 2]);
        assert_eq!(accepted(NoConsent::from_code), [1, 2, 3]);
        assert_eq!(accepted(Storage::from_code), [0, 1]);
        let storage: Vec<u8> = Storage::ALL.iter().map(|each| each.code()).collect();
        assert_eq!(storage, [0, 1]);
        let ended: Vec<u8> = Ended::ALL.iter().map(|why| why.code()).collect();
        let outcomes: Vec<u8> = Outcome::ALL.iter().map(|outcome| outcome.code()).collect();
        let refusals: Vec<u8> = NoConsent::ALL.iter().map(|why| why.code()).collect();
        assert_eq!(ended, [1, 2, 3, 4]);
        assert_eq!(outcomes, [1, 2]);
        assert_eq!(refusals, [1, 2, 3]);
        // Each code byte is the last byte of its message.
        for code in 0..=u8::MAX {
            let mut bytes = Report::Review(Box::new(widest())).encode();
            *bytes.last_mut().unwrap() = code;
            assert_eq!(Report::decode(&bytes).is_ok(), code <= 1, "{code}");
            let mut bytes = Report::Ended([2; 32], Ended::Withdrawn).encode();
            *bytes.last_mut().unwrap() = code;
            assert_eq!(
                Report::decode(&bytes).is_ok(),
                (1..=4).contains(&code),
                "{code}"
            );
            let mut bytes = Report::Finished([2; 32], Outcome::Complete).encode();
            *bytes.last_mut().unwrap() = code;
            assert_eq!(
                Report::decode(&bytes).is_ok(),
                (1..=2).contains(&code),
                "{code}"
            );
            let mut bytes = Answer::Declined([2; 32], NoConsent::Declined).encode();
            *bytes.last_mut().unwrap() = code;
            assert_eq!(
                Answer::decode(&bytes).is_ok(),
                (1..=3).contains(&code),
                "{code}"
            );
        }
        for tag in 0..=u8::MAX {
            let mut bytes = Report::Started([2; 32]).encode();
            bytes[0] = tag;
            assert_eq!(Report::decode(&bytes).is_ok(), tag == STARTED, "{tag}");
            let mut bytes = Answer::Consent([2; 32]).encode();
            bytes[0] = tag;
            assert_eq!(Answer::decode(&bytes).is_ok(), tag == CONSENT, "{tag}");
        }
    }

    #[test]
    fn a_zero_nonce_names_no_review() {
        for bytes in [
            Report::Started([0; 32]).encode(),
            Report::Ended([0; 32], Ended::NotConsented).encode(),
            Report::Finished([0; 32], Outcome::Complete).encode(),
        ] {
            assert!(Report::decode(&bytes).is_err());
        }
        assert!(Answer::decode(&Answer::Consent([0; 32]).encode()).is_err());
        for why in NoConsent::ALL {
            assert!(Answer::decode(&Answer::Declined([0; 32], *why).encode()).is_err());
        }
        let mut review = Report::Review(Box::new(widest())).encode();
        review[1..33].fill(0);
        assert!(Report::decode(&review).is_err());
    }

    #[test]
    fn review_fields_are_bounded_and_the_disk_is_a_kernel_name() {
        let fields = || ReviewFields {
            nonce: [1; 32],
            disk: "nvme0n1",
            capacity: 1,
            model: None,
            serial: None,
            hostname: "td",
            username: "alice",
            deployment: [0; 32],
            storage: Storage::Unencrypted,
        };
        assert!(Review::new(fields()).is_ok());
        let long_disk = "n".repeat(DISK_BYTES + 1);
        let long_label = "m".repeat(LABEL_BYTES + 1);
        let long_host = "h".repeat(HOSTNAME_BYTES + 1);
        let long_user = "u".repeat(USERNAME_BYTES + 1);
        for candidate in [
            ReviewFields {
                nonce: [0; 32],
                ..fields()
            },
            ReviewFields {
                disk: "",
                ..fields()
            },
            ReviewFields {
                disk: "../sda",
                ..fields()
            },
            ReviewFields {
                disk: "sda1 ",
                ..fields()
            },
            ReviewFields {
                disk: &long_disk,
                ..fields()
            },
            ReviewFields {
                capacity: 0,
                ..fields()
            },
            ReviewFields {
                model: Some(&long_label),
                ..fields()
            },
            ReviewFields {
                serial: Some(&long_label),
                ..fields()
            },
            ReviewFields {
                hostname: "",
                ..fields()
            },
            ReviewFields {
                hostname: &long_host,
                ..fields()
            },
            ReviewFields {
                username: "",
                ..fields()
            },
            ReviewFields {
                username: &long_user,
                ..fields()
            },
        ] {
            assert!(Review::new(candidate).is_err());
        }
    }

    #[test]
    fn a_review_decode_admits_only_what_construction_admits() {
        // An over-bound text spliced into an otherwise valid frame.
        let review = Review::new(ReviewFields {
            nonce: [1; 32],
            disk: "vda",
            capacity: 1,
            model: None,
            serial: None,
            hostname: "td",
            username: "alice",
            deployment: [0; 32],
            storage: Storage::Unencrypted,
        })
        .unwrap();
        let bytes = Report::Review(Box::new(review)).encode();
        // nonce 32, disk 1+3, capacity 8, model 1, serial 1, then hostname.
        let at = 1 + 32 + 4 + 8 + 2;
        assert_eq!(bytes[at], 2);
        let mut long = bytes[..at].to_vec();
        long.push(HOSTNAME_BYTES as u8 + 1);
        long.extend(std::iter::repeat_n(b'h', HOSTNAME_BYTES + 1));
        long.extend_from_slice(&bytes[at + 3..]);
        assert!(Report::decode(&long).is_err());
        let mut presence = bytes.clone();
        presence[at - 2] = 2;
        assert!(Report::decode(&presence).is_err());
        let mut invalid = bytes;
        invalid[at + 1] = 0xff;
        assert!(Report::decode(&invalid).is_err());
    }
}
