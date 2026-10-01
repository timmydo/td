//! Immutable public request description shared with the trusted renderer.

const MAGIC: &[u8; 8] = b"TDCONS01";
const MAX_BYTES: usize = 256;
// Whole-disk installation summary bounds, in displayed (escaped) bytes.
pub const DISK_NAME_BYTES: usize = 31;
pub const MODEL_WIDTH: usize = 32;
pub const SERIAL_WIDTH: usize = 24;
pub const HOSTNAME_BYTES: usize = 63;
pub const USERNAME_BYTES: usize = 32;
const ABSENT_LABEL: u8 = 0xff;
const TRUNCATED_LABEL: u8 = 0x80;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Primary,
    Recovery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Recovery {
    SecondToken,
    Unrecoverable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Enrollment {
    CreatePrimary,
    ProvePrimary,
    CreateRecovery,
    ProveRecovery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    TpmPcr7,
}

/// A disk label as the trusted renderer shows it: printable ASCII other than
/// space and backslash as itself, backslash as `\\`, space as `\s`, every
/// other byte as `\xNN`, cut before the first escape that would pass the
/// field's width. A space is escaped so a label cannot imitate an indented
/// continuation or a fixed line; `\s` keeps ordinary model names whole.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Label {
    text: String,
    truncated: bool,
}

impl Label {
    /// A disk's reported model, escaped to its field.
    pub fn model(raw: &[u8]) -> Self {
        Self::escape(raw, MODEL_WIDTH)
    }

    /// A disk's reported serial, escaped to its field.
    pub fn serial(raw: &[u8]) -> Self {
        Self::escape(raw, SERIAL_WIDTH)
    }

    fn escape(raw: &[u8], width: usize) -> Self {
        use std::fmt::Write as _;
        let mut text = String::with_capacity(width);
        let mut truncated = false;
        for byte in raw {
            let unit = if displayed(*byte) {
                1
            } else if matches!(*byte, b'\\' | b' ') {
                2
            } else {
                4
            };
            if text.len().saturating_add(unit) > width {
                truncated = true;
                break;
            }
            // Writing to a String cannot fail.
            let _ = match *byte {
                b'\\' => text.write_str("\\\\"),
                b' ' => text.write_str("\\s"),
                byte if displayed(byte) => text.write_char(char::from(byte)),
                byte => write!(text, "\\x{byte:02x}"),
            };
        }
        Self { text, truncated }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// What `escape` emits for some input within `width`; a truncated label
    /// was cut only where no further escape fitted (each is at most 4 bytes).
    fn canonical(&self, width: usize) -> bool {
        if self.text.len() > width || (self.truncated && self.text.len().saturating_add(4) <= width)
        {
            return false;
        }
        let mut rest = self.text.as_bytes();
        loop {
            rest = match rest {
                [] => return true,
                [b'\\', b'\\' | b's', tail @ ..] => tail,
                [b'\\', b'x', high, low, tail @ ..] => match (hex_digit(*high), hex_digit(*low)) {
                    (Some(high), Some(low)) if escaped(high << 4 | low) => tail,
                    _ => return false,
                },
                [byte, tail @ ..] if displayed(*byte) => tail,
                _ => return false,
            };
        }
    }
}

/// Bytes a label shows as themselves.
fn displayed(byte: u8) -> bool {
    byte.is_ascii_graphic() && byte != b'\\'
}

/// Bytes a label shows as `\xNN`; a backslash and a space have their own.
fn escaped(byte: u8) -> bool {
    !displayed(byte) && !matches!(byte, b'\\' | b' ')
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Operation {
    Install {
        deployment: String,
        requester: u32,
    },
    /// Erase one whole disk and install the live medium's deployment on it.
    /// A fixed summary of the plan; the plan itself is bound elsewhere.
    InstallDisk {
        requester: u32,
        disk: String,
        capacity: u64,
        model: Option<Label>,
        serial: Option<Label>,
        hostname: String,
        username: String,
        /// The first eight bytes of the deployment ID, shown as hex.
        deployment: [u8; 8],
    },
    Enroll {
        platform: Platform,
        recovery: Recovery,
        step: Enrollment,
    },
    Unlock {
        role: Role,
    },
    Set {
        role: Role,
        application: String,
        name: String,
        application_uid: u32,
        // Pin the authenticated requester separately; this profile admits only its owner.
        requester: u32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    nonce: [u8; 32],
    owner: u32,
    operation: Operation,
}

impl Request {
    /// The authority supplies fresh entropy and independently admitted identities.
    pub fn new(nonce: [u8; 32], owner: u32, operation: Operation) -> Result<Self, String> {
        if nonce == [0; 32] || !(1000..=65533).contains(&owner) {
            return Err("invalid consent request identity".into());
        }
        match &operation {
            Operation::Install {
                deployment,
                requester,
            } if !deployment_id(deployment) || *requester != owner => {
                return Err("invalid consent installation target".into());
            }
            Operation::InstallDisk {
                requester,
                disk,
                capacity,
                model,
                serial,
                hostname,
                username,
                deployment: _,
            } if *requester != owner
                || !disk_name(disk)
                || *capacity == 0
                || !model
                    .as_ref()
                    .is_none_or(|label| label.canonical(MODEL_WIDTH))
                || !serial
                    .as_ref()
                    .is_none_or(|label| label.canonical(SERIAL_WIDTH))
                || !shown_name(hostname, HOSTNAME_BYTES)
                || !shown_name(username, USERNAME_BYTES) =>
            {
                return Err("invalid consent disk installation summary".into());
            }
            Operation::Enroll {
                platform: _,
                recovery: Recovery::Unrecoverable,
                step: Enrollment::CreateRecovery | Enrollment::ProveRecovery,
            } => return Err("unrecoverable enrollment cannot enroll a recovery token".into()),
            Operation::Set {
                role: _,
                application,
                name,
                application_uid,
                requester,
            } if !application_name(application)
                || !secret_name(name)
                || !(65536..=2147483647).contains(application_uid)
                || *requester != owner =>
            {
                return Err("invalid consent credential target".into());
            }
            _ => {}
        }
        Ok(Self {
            nonce,
            owner,
            operation,
        })
    }

    pub fn nonce(&self) -> &[u8; 32] {
        &self.nonce
    }
    pub fn owner(&self) -> u32 {
        self.owner
    }
    pub fn operation(&self) -> &Operation {
        &self.operation
    }

    /// The only next presentation in this same enrollment operation.
    pub fn following_enrollment_step(&self) -> Result<Option<Self>, String> {
        let Operation::Enroll {
            platform,
            recovery,
            step,
        } = &self.operation
        else {
            return Ok(None);
        };
        let next = match (*step, *recovery) {
            (Enrollment::CreatePrimary, _) => Enrollment::ProvePrimary,
            (Enrollment::ProvePrimary, Recovery::SecondToken) => Enrollment::CreateRecovery,
            (Enrollment::CreateRecovery, Recovery::SecondToken) => Enrollment::ProveRecovery,
            (Enrollment::ProvePrimary, Recovery::Unrecoverable)
            | (Enrollment::ProveRecovery, Recovery::SecondToken) => return Ok(None),
            _ => return Err("invalid enrollment progression".into()),
        };
        Self::new(
            self.nonce,
            self.owner,
            Operation::Enroll {
                platform: *platform,
                recovery: *recovery,
                step: next,
            },
        )
        .map(Some)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(MAX_BYTES);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&self.nonce);
        bytes.extend_from_slice(&self.owner.to_be_bytes());
        match &self.operation {
            Operation::Install {
                deployment,
                requester,
            } => {
                bytes.push(5);
                bytes.extend_from_slice(&requester.to_be_bytes());
                bytes.extend_from_slice(deployment.as_bytes());
            }
            Operation::InstallDisk {
                requester,
                disk,
                capacity,
                model,
                serial,
                hostname,
                username,
                deployment,
            } => {
                bytes.push(6);
                bytes.extend_from_slice(&requester.to_be_bytes());
                bytes.extend_from_slice(&capacity.to_be_bytes());
                // Construction bounds every string below 128 bytes.
                put_text(&mut bytes, disk);
                put_label(&mut bytes, model.as_ref());
                put_label(&mut bytes, serial.as_ref());
                put_text(&mut bytes, hostname);
                put_text(&mut bytes, username);
                bytes.extend_from_slice(deployment);
            }
            Operation::Enroll {
                platform,
                recovery,
                step,
            } => {
                bytes.push(1);
                bytes.push(match platform {
                    Platform::TpmPcr7 => 1,
                });
                bytes.push(match recovery {
                    Recovery::SecondToken => 1,
                    Recovery::Unrecoverable => 0,
                });
                bytes.push(match step {
                    Enrollment::CreatePrimary => 1,
                    Enrollment::ProvePrimary => 2,
                    Enrollment::CreateRecovery => 3,
                    Enrollment::ProveRecovery => 4,
                });
            }
            Operation::Unlock { role } => {
                bytes.push(2);
                bytes.push(match role {
                    Role::Primary => 1,
                    Role::Recovery => 2,
                });
            }
            Operation::Set {
                role,
                application,
                name,
                application_uid,
                requester,
            } => {
                bytes.push(4);
                bytes.push(match role {
                    Role::Primary => 1,
                    Role::Recovery => 2,
                });
                bytes.extend_from_slice(&application_uid.to_be_bytes());
                bytes.extend_from_slice(&requester.to_be_bytes());
                // Construction bounds both strings to 64 ASCII bytes.
                bytes.push(application.len() as u8);
                bytes.extend_from_slice(application.as_bytes());
                bytes.push(name.len() as u8);
                bytes.extend_from_slice(name.as_bytes());
            }
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_BYTES {
            return Err("oversized consent request".into());
        }
        let mut input = Input(bytes);
        if input.take(8)? != MAGIC {
            return Err("unsupported consent request version".into());
        }
        let nonce = input
            .take(32)?
            .try_into()
            .map_err(|_| "invalid consent nonce")?;
        let owner = input.number()?;
        let operation = match input.byte()? {
            5 => Operation::Install {
                requester: input.number()?,
                deployment: String::from_utf8(input.take(64)?.to_vec())
                    .map_err(|_| "invalid deployment ID encoding")?,
            },
            6 => {
                let requester = input.number()?;
                let capacity = input.wide()?;
                Operation::InstallDisk {
                    requester,
                    capacity,
                    disk: input.text()?,
                    model: input.label()?,
                    serial: input.label()?,
                    hostname: input.text()?,
                    username: input.text()?,
                    deployment: input
                        .take(8)?
                        .try_into()
                        .map_err(|_| "truncated deployment prefix")?,
                }
            }
            1 => {
                let platform = match input.byte()? {
                    1 => Platform::TpmPcr7,
                    _ => return Err("unsupported consent platform profile".into()),
                };
                let recovery = match input.byte()? {
                    1 => Recovery::SecondToken,
                    0 => Recovery::Unrecoverable,
                    _ => return Err("invalid consent recovery policy".into()),
                };
                let step = match input.byte()? {
                    1 => Enrollment::CreatePrimary,
                    2 => Enrollment::ProvePrimary,
                    3 => Enrollment::CreateRecovery,
                    4 => Enrollment::ProveRecovery,
                    _ => return Err("invalid consent enrollment step".into()),
                };
                Operation::Enroll {
                    platform,
                    recovery,
                    step,
                }
            }
            2 => Operation::Unlock {
                role: match input.byte()? {
                    1 => Role::Primary,
                    2 => Role::Recovery,
                    _ => return Err("invalid consent token role".into()),
                },
            },
            4 => {
                let role = match input.byte()? {
                    1 => Role::Primary,
                    2 => Role::Recovery,
                    _ => return Err("invalid write token role".into()),
                };
                let application_uid = input.number()?;
                let requester = input.number()?;
                Operation::Set {
                    role,
                    application: input.text()?,
                    name: input.text()?,
                    application_uid,
                    requester,
                }
            }
            _ => return Err("unknown consent operation".into()),
        };
        if !input.0.is_empty() {
            return Err("trailing consent request bytes".into());
        }
        Self::new(nonce, owner, operation)
    }

    /// Display human-relevant operation arguments; retain the nonce without displaying it.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![
            "TD SECURE ATTENTION".into(),
            format!("SESSION USER {}", self.owner),
        ];
        match &self.operation {
            Operation::Install { deployment, .. } => {
                lines.push("INSTALL BUILT SYSTEM".into());
                lines.push(format!("DEPLOYMENT: {deployment}"));
                lines.push("PREVIOUS SYSTEM KEPT FOR ROLLBACK".into());
                lines.push("RESTART REQUIRED TO USE THIS SYSTEM".into());
                lines.push("ENTER: INSTALL   ESC: CANCEL".into());
            }
            Operation::InstallDisk {
                requester: _,
                disk,
                capacity,
                model,
                serial,
                hostname,
                username,
                deployment,
            } => {
                lines.push("ERASE DISK AND INSTALL TD".into());
                lines.push(format!("DISK: {disk}"));
                lines.push(format!(
                    "SIZE: {}.{} GB, {capacity} BYTES",
                    capacity / 1_000_000_000,
                    capacity / 100_000_000 % 10
                ));
                lines.push(label_line("MODEL", model.as_ref()));
                lines.push(label_line("SERIAL", serial.as_ref()));
                lines.push("ALL DATA ON THIS DISK WILL BE LOST".into());
                lines.push(format!("HOSTNAME: {hostname}"));
                lines.push(format!("USER: {username}"));
                let mut prefix = String::with_capacity(16);
                for byte in deployment {
                    use std::fmt::Write as _;
                    // Writing to a String cannot fail.
                    let _ = write!(prefix, "{byte:02x}");
                }
                lines.push(format!("DEPLOYMENT: {prefix}..."));
                lines.push("UNENCRYPTED STORAGE, AUTOMATIC LOGIN".into());
                lines.push("ENTER: ERASE AND INSTALL   ESC: CANCEL".into());
            }
            Operation::Enroll {
                platform,
                recovery,
                step,
            } => {
                lines.push("ENROLL SECRET STORE".into());
                lines.push(
                    match platform {
                        Platform::TpmPcr7 => "PLATFORM: TPM PCR 7",
                    }
                    .into(),
                );
                lines.push(
                    match recovery {
                        Recovery::SecondToken => "RECOVERY: SECOND RECOVERY TOKEN REQUIRED",
                        Recovery::Unrecoverable => "RECOVERY: NONE. LOST TOKEN LOSES SECRETS",
                    }
                    .into(),
                );
                lines.push(
                    match step {
                        Enrollment::CreatePrimary => "CREATE PRIMARY TOKEN CREDENTIAL",
                        Enrollment::ProvePrimary => "VERIFY PRIMARY TOKEN CREDENTIAL",
                        Enrollment::CreateRecovery => "CREATE RECOVERY TOKEN CREDENTIAL",
                        Enrollment::ProveRecovery => "VERIFY RECOVERY TOKEN CREDENTIAL",
                    }
                    .into(),
                );
                lines.push(
                    match step {
                        Enrollment::CreatePrimary | Enrollment::ProvePrimary => {
                            "TOUCH THE PRIMARY TOKEN"
                        }
                        Enrollment::CreateRecovery | Enrollment::ProveRecovery => {
                            "TOUCH THE RECOVERY TOKEN"
                        }
                    }
                    .into(),
                );
            }
            Operation::Unlock { role } => {
                lines.push("UNLOCK SECRET STORE FOR THIS SESSION".into());
                lines.push(
                    match role {
                        Role::Primary => "TOUCH THE PRIMARY TOKEN",
                        Role::Recovery => "TOUCH THE RECOVERY TOKEN",
                    }
                    .into(),
                );
            }
            Operation::Set {
                role,
                application,
                name,
                application_uid,
                requester,
            } => {
                lines.push("SET ONE APPLICATION CREDENTIAL".into());
                lines.push(format!("REQUESTER UID {requester}"));
                lines.push(format!("APPLICATION UID {application_uid}"));
                lines.push(format!("APPLICATION: {application}"));
                lines.push(format!("CREDENTIAL: {name}"));
                lines.push(
                    match role {
                        Role::Primary => "TOUCH THE PRIMARY TOKEN",
                        Role::Recovery => "TOUCH THE RECOVERY TOKEN",
                    }
                    .into(),
                );
            }
        }
        if !matches!(
            self.operation,
            Operation::Install { .. } | Operation::InstallDisk { .. }
        ) {
            lines.push("ESC TO CANCEL".into());
        }
        lines
    }
}

pub(crate) fn application_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && text.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_.".contains(&byte)
        })
}

/// A kernel disk name, as the installation plan admits one.
fn disk_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= DISK_NAME_BYTES
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
}

/// A name shown verbatim: printable ASCII without space or backslash, so it
/// cannot imitate an indented continuation, a fixed line or an escape.
fn shown_name(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && text.bytes().all(displayed)
}

fn label_line(field: &str, label: Option<&Label>) -> String {
    match label {
        None => format!("{field}: NOT REPORTED"),
        Some(label) if label.text().is_empty() => format!("{field}: REPORTED EMPTY"),
        Some(label) if label.truncated() => format!("{field}: {} (TRUNCATED)", label.text()),
        Some(label) => format!("{field}: {}", label.text()),
    }
}

fn put_text(bytes: &mut Vec<u8>, text: &str) {
    bytes.push(text.len() as u8);
    bytes.extend_from_slice(text.as_bytes());
}

fn put_label(bytes: &mut Vec<u8>, label: Option<&Label>) {
    match label {
        None => bytes.push(ABSENT_LABEL),
        Some(label) => {
            let length = label.text.len() as u8;
            bytes.push(if label.truncated {
                length | TRUNCATED_LABEL
            } else {
                length
            });
            bytes.extend_from_slice(label.text.as_bytes());
        }
    }
}

fn secret_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
}

struct Input<'a>(&'a [u8]);
impl<'a> Input<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let bytes = self.0.get(..count).ok_or("truncated consent request")?;
        self.0 = self.0.get(count..).ok_or("truncated consent request")?;
        Ok(bytes)
    }
    fn byte(&mut self) -> Result<u8, String> {
        self.take(1)?
            .first()
            .copied()
            .ok_or_else(|| "truncated consent byte".into())
    }
    fn number(&mut self) -> Result<u32, String> {
        Ok(u32::from_be_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| "truncated consent number")?,
        ))
    }
    fn text(&mut self) -> Result<String, String> {
        let length = usize::from(self.byte()?);
        String::from_utf8(self.take(length)?.to_vec()).map_err(|_| "invalid consent text".into())
    }
    fn wide(&mut self) -> Result<u64, String> {
        Ok(u64::from_be_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| "truncated consent number")?,
        ))
    }
    fn label(&mut self) -> Result<Option<Label>, String> {
        let head = self.byte()?;
        if head == ABSENT_LABEL {
            return Ok(None);
        }
        let length = usize::from(head & !TRUNCATED_LABEL);
        Ok(Some(Label {
            text: String::from_utf8(self.take(length)?.to_vec())
                .map_err(|_| "invalid consent label")?,
            truncated: head & TRUNCATED_LABEL != 0,
        }))
    }
}

pub(crate) fn deployment_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn enrollment_progression_preserves_identity_and_stops_at_the_selected_final_proof() {
        for recovery in [Recovery::SecondToken, Recovery::Unrecoverable] {
            let mut request = Request::new(
                [42; 32],
                1000,
                Operation::Enroll {
                    platform: Platform::TpmPcr7,
                    recovery,
                    step: Enrollment::CreatePrimary,
                },
            )
            .unwrap();
            let mut expected = vec![Enrollment::ProvePrimary];
            if recovery == Recovery::SecondToken {
                expected.extend([Enrollment::CreateRecovery, Enrollment::ProveRecovery]);
            }
            for step in expected {
                request = request.following_enrollment_step().unwrap().unwrap();
                assert_eq!(
                    request,
                    Request::new(
                        [42; 32],
                        1000,
                        Operation::Enroll {
                            platform: Platform::TpmPcr7,
                            recovery,
                            step,
                        }
                    )
                    .unwrap()
                );
            }
            assert_eq!(request.following_enrollment_step().unwrap(), None);
        }
        let unlock = Request::new(
            [42; 32],
            1000,
            Operation::Unlock {
                role: Role::Primary,
            },
        )
        .unwrap();
        assert_eq!(unlock.following_enrollment_step().unwrap(), None);
    }

    #[test]
    fn installation_has_one_canonical_id_and_the_requester_must_be_its_owner() {
        for id in [
            "a".repeat(63),
            "a".repeat(65),
            "A".repeat(64),
            "g".repeat(64),
            format!("{}\n", "a".repeat(63)),
        ] {
            assert!(Request::new(
                [1; 32],
                1000,
                Operation::Install {
                    deployment: id,
                    requester: 1000
                }
            )
            .is_err());
        }
        assert!(Request::new(
            [1; 32],
            1000,
            Operation::Install {
                deployment: "a".repeat(64),
                requester: 1001
            }
        )
        .is_err());
        let request = Request::new(
            [1; 32],
            1000,
            Operation::Install {
                deployment: "a".repeat(64),
                requester: 1000,
            },
        )
        .unwrap();
        let mut literal = b"TDCONS01".to_vec();
        literal.extend([1; 32]);
        literal.extend([0, 0, 3, 232, 5, 0, 0, 3, 232]);
        literal.extend([b'a'; 64]);
        assert_eq!(request.encode(), literal);
        assert!(request
            .lines()
            .iter()
            .any(|line| line == &format!("DEPLOYMENT: {}", "a".repeat(64))));
        assert!(request
            .lines()
            .iter()
            .any(|line| line == "ENTER: INSTALL   ESC: CANCEL"));
    }

    fn disk(model: Option<Label>, serial: Option<Label>) -> Operation {
        Operation::InstallDisk {
            requester: 1000,
            disk: "nvme0n1".into(),
            capacity: 512_110_190_592,
            model,
            serial,
            hostname: "td".into(),
            username: "tester".into(),
            deployment: [0xab, 0xcd, 0, 1, 2, 3, 4, 0xff],
        }
    }

    /// Every field at its bound: the largest request this profile encodes.
    fn widest_disk() -> Operation {
        Operation::InstallDisk {
            requester: 1000,
            disk: "d".repeat(DISK_NAME_BYTES),
            capacity: u64::MAX,
            model: Some(Label::escape(&[0xff; 64], MODEL_WIDTH)),
            serial: Some(Label::escape(&[b'\\'; 64], SERIAL_WIDTH)),
            hostname: "h".repeat(HOSTNAME_BYTES),
            username: "u".repeat(USERNAME_BYTES),
            deployment: [0xff; 8],
        }
    }

    #[test]
    fn labels_escape_every_byte_a_line_could_be_forged_with() {
        let label = Label::escape(b"Disk A\\b\n\xc3\xa9~", 64);
        assert_eq!(label.text(), "Disk\\sA\\\\b\\x0a\\xc3\\xa9~");
        assert!(!label.truncated());
        assert!(label.canonical(64));
        // Cut before the escape that would pass the width, never inside it.
        let cut = Label::escape(b"ab\x00", 5);
        assert_eq!((cut.text(), cut.truncated()), ("ab", true));
        assert!(cut.canonical(5));
        let exact = Label::escape(b"ab\x00", 6);
        assert_eq!((exact.text(), exact.truncated()), ("ab\\x00", false));
        let canonical = |text: &str, truncated, width| {
            Label {
                text: text.into(),
                truncated,
            }
            .canonical(width)
        };
        for bad in [
            "a b", "\\", "\\x41", "\\x5c", "\\x20", "\\xC3", "\\x0", "\\q", "a\nb", "\u{e9}",
        ] {
            assert!(!canonical(bad, false, 64), "{bad:?}");
        }
        assert!(canonical("\\\\\\s", false, 64));
        // Truncated with room for another escape is not a cut escape made.
        assert!(!canonical("ab", true, 6));
        assert!(canonical("ab", true, 5));
        assert!(!canonical("abc", false, 2));
    }

    #[test]
    fn disk_installation_shows_its_whole_fixed_summary() {
        let request = Request::new(
            [7; 32],
            1000,
            disk(Some(Label::escape(b"Samsung SSD 980", MODEL_WIDTH)), None),
        )
        .unwrap();
        assert_eq!(
            request.lines(),
            [
                "TD SECURE ATTENTION",
                "SESSION USER 1000",
                "ERASE DISK AND INSTALL TD",
                "DISK: nvme0n1",
                "SIZE: 512.1 GB, 512110190592 BYTES",
                "MODEL: Samsung\\sSSD\\s980",
                "SERIAL: NOT REPORTED",
                "ALL DATA ON THIS DISK WILL BE LOST",
                "HOSTNAME: td",
                "USER: tester",
                "DEPLOYMENT: abcd0001020304ff...",
                "UNENCRYPTED STORAGE, AUTOMATIC LOGIN",
                "ENTER: ERASE AND INSTALL   ESC: CANCEL",
            ]
        );
        let mut literal = b"TDCONS01".to_vec();
        literal.extend([7; 32]);
        literal.extend([0, 0, 3, 232, 6, 0, 0, 3, 232]);
        literal.extend(512_110_190_592_u64.to_be_bytes());
        literal.push(7);
        literal.extend(b"nvme0n1");
        literal.push(17);
        literal.extend(b"Samsung\\sSSD\\s980");
        literal.push(0xff);
        literal.push(2);
        literal.extend(b"td");
        literal.push(6);
        literal.extend(b"tester");
        literal.extend([0xab, 0xcd, 0, 1, 2, 3, 4, 0xff]);
        assert_eq!(request.encode(), literal);

        let truncated = Request::new(
            [7; 32],
            1000,
            disk(None, Some(Label::escape(&[b'S'; 30], SERIAL_WIDTH))),
        )
        .unwrap();
        assert!(truncated
            .lines()
            .contains(&format!("SERIAL: {} (TRUNCATED)", "S".repeat(24))));
        let empty = Request::new([7; 32], 1000, disk(Some(Label::escape(b"", 32)), None)).unwrap();
        assert!(empty.lines().contains(&"MODEL: REPORTED EMPTY".to_string()));
        // A disk reporting a marker's text shows it as its own label: every
        // marker holds a space, which a label always escapes.
        let forged = Request::new(
            [7; 32],
            1000,
            disk(Some(Label::model(b"REPORTED EMPTY")), None),
        )
        .unwrap();
        assert!(forged
            .lines()
            .contains(&"MODEL: REPORTED\\sEMPTY".to_string()));
    }

    /// The wire is held to the same checks as construction.
    #[test]
    fn a_decoded_disk_installation_is_refused_where_construction_would_be() {
        let request = Request::new(
            [7; 32],
            1000,
            disk(Some(Label::model(b"ab")), Some(Label::serial(b"cd"))),
        )
        .unwrap();
        let bytes = request.encode();
        // Header 45, requester 4, capacity 8, disk name 1 + 7; then the model.
        let model = 45 + 4 + 8 + 8;
        assert_eq!(&bytes[model..model + 3], &[2, b'a', b'b']);
        for (offset, value) in [
            (model + 1, b' '),
            (model + 1, b'\\'),
            (model, 0x82),
            (58, b'/'),
        ] {
            let mut bad = bytes.clone();
            bad[offset] = value;
            assert!(Request::decode(&bad).is_err(), "{offset} {value}");
        }
        // A whole model one byte over its width, framed correctly.
        let mut over = bytes[..model].to_vec();
        over.push(33);
        over.extend([b'm'; 33]);
        over.extend(&bytes[model + 3..]);
        assert_eq!(
            Request::decode(&over).err().unwrap(),
            "invalid consent disk installation summary"
        );
        let mut zero = bytes.clone();
        zero[49..57].fill(0);
        assert!(Request::decode(&zero).is_err());
    }

    #[test]
    fn disk_installation_fits_the_request_bound_and_refuses_unshowable_names() {
        let widest = Request::new([7; 32], 1000, widest_disk()).unwrap();
        assert!(
            widest.encode().len() <= MAX_BYTES,
            "{}",
            widest.encode().len()
        );
        assert_eq!(Request::decode(&widest.encode()).unwrap(), widest);

        let make = |edit: fn(&mut Operation)| {
            let mut operation = disk(None, None);
            edit(&mut operation);
            Request::new([7; 32], 1000, operation)
        };
        assert!(make(|_| {}).is_ok());
        let refused: [fn(&mut Operation); 13] = [
            |op| {
                if let Operation::InstallDisk { requester, .. } = op {
                    *requester = 1001;
                }
            },
            |op| {
                if let Operation::InstallDisk { disk, .. } = op {
                    *disk = String::new();
                }
            },
            |op| {
                if let Operation::InstallDisk { disk, .. } = op {
                    *disk = "d".repeat(DISK_NAME_BYTES + 1);
                }
            },
            |op| {
                if let Operation::InstallDisk { disk, .. } = op {
                    *disk = "../sda".into();
                }
            },
            |op| {
                if let Operation::InstallDisk { capacity, .. } = op {
                    *capacity = 0;
                }
            },
            |op| {
                if let Operation::InstallDisk { hostname, .. } = op {
                    *hostname = String::new();
                }
            },
            |op| {
                if let Operation::InstallDisk { hostname, .. } = op {
                    *hostname = "h".repeat(HOSTNAME_BYTES + 1);
                }
            },
            |op| {
                if let Operation::InstallDisk { hostname, .. } = op {
                    *hostname = "td host".into();
                }
            },
            |op| {
                if let Operation::InstallDisk { hostname, .. } = op {
                    *hostname = "td\\s".into();
                }
            },
            |op| {
                if let Operation::InstallDisk { username, .. } = op {
                    *username = "u".repeat(USERNAME_BYTES + 1);
                }
            },
            |op| {
                if let Operation::InstallDisk { username, .. } = op {
                    *username = "t\u{e9}".into();
                }
            },
            |op| {
                if let Operation::InstallDisk { model, .. } = op {
                    *model = Some(Label::escape(&[b'm'; 40], 64));
                }
            },
            |op| {
                if let Operation::InstallDisk { serial, .. } = op {
                    *serial = Some(Label {
                        text: "a b".into(),
                        truncated: false,
                    });
                }
            },
        ];
        for (index, edit) in refused.into_iter().enumerate() {
            assert!(make(edit).is_err(), "case {index}");
        }
    }

    #[test]
    fn every_operation_roundtrips_and_refuses_truncation_or_trailing_bytes() {
        let mut operations = vec![
            Operation::Install {
                deployment: "ab".repeat(32),
                requester: 1000,
            },
            disk(None, None),
            disk(
                Some(Label::escape(b"Model \xff", MODEL_WIDTH)),
                Some(Label::escape(&[b'9'; 40], SERIAL_WIDTH)),
            ),
            widest_disk(),
            Operation::Unlock {
                role: Role::Primary,
            },
            Operation::Unlock {
                role: Role::Recovery,
            },
            Operation::Set {
                role: Role::Primary,
                application: "mail".into(),
                name: "Main".into(),
                application_uid: 65537,
                requester: 1000,
            },
        ];
        for recovery in [Recovery::SecondToken, Recovery::Unrecoverable] {
            for step in [
                Enrollment::CreatePrimary,
                Enrollment::ProvePrimary,
                Enrollment::CreateRecovery,
                Enrollment::ProveRecovery,
            ] {
                let operation = Operation::Enroll {
                    platform: Platform::TpmPcr7,
                    recovery,
                    step,
                };
                if recovery == Recovery::Unrecoverable
                    && matches!(step, Enrollment::CreateRecovery | Enrollment::ProveRecovery)
                {
                    assert!(Request::new([1; 32], 1000, operation).is_err());
                } else {
                    operations.push(operation);
                }
            }
        }
        let mut encodings = std::collections::BTreeSet::new();
        for operation in operations {
            let request = Request::new([1; 32], 1000, operation).unwrap();
            let mut bytes = request.encode();
            assert_eq!(Request::decode(&bytes).unwrap(), request);
            assert!(encodings.insert(bytes.clone()));
            for end in 0..bytes.len() {
                assert!(Request::decode(&bytes[..end]).is_err());
            }
            bytes.push(0);
            assert!(Request::decode(&bytes).is_err());
            assert!(request
                .lines()
                .iter()
                .all(|line| line.is_ascii() && !line.contains('\n')));
        }
    }

    #[test]
    fn wire_version_identity_and_token_role_have_independent_literal_encoding() {
        let request = Request::new(
            [42; 32],
            1000,
            Operation::Unlock {
                role: Role::Recovery,
            },
        )
        .unwrap();
        let mut literal = b"TDCONS01".to_vec();
        literal.extend([42; 32]);
        literal.extend([0, 0, 3, 232, 2, 2]);
        assert_eq!(request.encode(), literal);
        for owner in [0, 991, 999, 65534, 65536, u32::MAX] {
            assert!(Request::new(
                [1; 32],
                owner,
                Operation::Unlock {
                    role: Role::Primary
                }
            )
            .is_err());
        }
        assert!(Request::new(
            [0; 32],
            1000,
            Operation::Unlock {
                role: Role::Primary
            }
        )
        .is_err());
        for index in [0, 44, 45] {
            let mut bad = literal.clone();
            bad[index] = 255;
            assert!(Request::decode(&bad).is_err());
        }
        assert!(Request::decode(&[0; MAX_BYTES + 1]).is_err());
    }

    #[test]
    fn enrollment_profile_is_bound_by_an_independent_wire_tag() {
        let request = Request::new(
            [1; 32],
            1000,
            Operation::Enroll {
                platform: Platform::TpmPcr7,
                recovery: Recovery::SecondToken,
                step: Enrollment::CreatePrimary,
            },
        )
        .unwrap();
        let mut bytes = request.encode();
        assert_eq!(&bytes[44..], &[1, 1, 1, 1]);
        assert!(request
            .lines()
            .iter()
            .any(|line| line == "PLATFORM: TPM PCR 7"));
        bytes[45] = 2;
        assert!(Request::decode(&bytes).is_err());
    }

    #[test]
    fn credential_display_keeps_every_public_argument() {
        let request = Request::new(
            [42; 32],
            1001,
            Operation::Set {
                role: Role::Primary,
                application: "mail".into(),
                name: "Main_Account-2026".into(),
                application_uid: 65537,
                requester: 1001,
            },
        )
        .unwrap();
        assert_eq!(
            request.lines(),
            [
                "TD SECURE ATTENTION",
                "SESSION USER 1001",
                "SET ONE APPLICATION CREDENTIAL",
                "REQUESTER UID 1001",
                "APPLICATION UID 65537",
                "APPLICATION: mail",
                "CREDENTIAL: Main_Account-2026",
                "TOUCH THE PRIMARY TOKEN",
                "ESC TO CANCEL",
            ]
        );
        let enrollment = Request::new(
            [1; 32],
            1000,
            Operation::Enroll {
                platform: Platform::TpmPcr7,
                recovery: Recovery::Unrecoverable,
                step: Enrollment::ProvePrimary,
            },
        )
        .unwrap();
        assert_eq!(&enrollment.encode()[44..], &[1, 1, 0, 2]);
        assert_eq!(
            enrollment.lines(),
            [
                "TD SECURE ATTENTION",
                "SESSION USER 1000",
                "ENROLL SECRET STORE",
                "PLATFORM: TPM PCR 7",
                "RECOVERY: NONE. LOST TOKEN LOSES SECRETS",
                "VERIFY PRIMARY TOKEN CREDENTIAL",
                "TOUCH THE PRIMARY TOKEN",
                "ESC TO CANCEL",
            ]
        );
    }

    #[test]
    fn target_is_bounded_and_case_sensitive_without_control_characters() {
        let make = |app: &str, name: &str, uid, requester| {
            Request::new(
                [1; 32],
                1000,
                Operation::Set {
                    role: Role::Primary,
                    application: app.into(),
                    name: name.into(),
                    application_uid: uid,
                    requester,
                },
            )
        };
        let upper = make("mail", "Main", 65537, 1000).unwrap();
        let lower = make("mail", "main", 65537, 1000).unwrap();
        assert_ne!(upper.encode(), lower.encode());
        assert_ne!(upper.lines(), lower.lines());
        for app in ["", "Mail", "../mail", "mail\n", "mail/other"] {
            assert!(make(app, "main", 65537, 1000).is_err());
        }
        for name in [
            "",
            "../main",
            "main\n",
            "main/other",
            "main other",
            "main.other",
        ] {
            assert!(make("mail", name, 65537, 1000).is_err());
        }
        assert!(make(&"a".repeat(65), "main", 65537, 1000).is_err());
        assert!(make("mail", &"a".repeat(65), 65537, 1000).is_err());
        assert!(make("mail", "main", 1000, 1000).is_err());
        assert!(make("mail", "main", 65537, 1001).is_err());
        assert!(
            make(&"a".repeat(64), &"A".repeat(64), 65537, 1000)
                .unwrap()
                .encode()
                .len()
                <= MAX_BYTES
        );
    }
}
