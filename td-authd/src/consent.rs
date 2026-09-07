//! Immutable public request description shared with the trusted renderer.

const MAGIC: &[u8; 8] = b"TDCONS01";
const MAX_BYTES: usize = 256;

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Operation {
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
        lines.push("ESC TO CANCEL".into());
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
    fn every_operation_roundtrips_and_refuses_truncation_or_trailing_bytes() {
        let mut operations = vec![
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
