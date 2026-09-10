//! Bounded public target description. Credential bytes travel separately.

use crate::consent::{Operation, Request, Role};

pub(crate) const SOCKET: &str = "/run/td-authd/1000/set";
pub(crate) const GREETING: &[u8; 8] = b"TDSET02\n";
pub(crate) const ADMITTED: u8 = 2;
pub(crate) const LIMIT: usize = 132;
pub(crate) const MAX_SECRET: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub app: String,
    pub name: String,
    pub role: Role,
}

impl Target {
    pub fn parse(target: &str, role: Role) -> Result<Self, String> {
        let (app, name) = target.split_once('/').ok_or("expected APPLICATION/NAME")?;
        Self::parts(app, name, role)
    }

    fn parts(app: &str, name: &str, role: Role) -> Result<Self, String> {
        let value = Self {
            app: app.into(),
            name: name.into(),
            role,
        };
        // Reuse the canonical consent grammar, without granting this dummy
        // identity or nonce any authority. Root binds the actual values.
        value.operation(1000, 65536)?;
        Ok(value)
    }

    pub fn operation(&self, owner: u32, application_uid: u32) -> Result<Operation, String> {
        let operation = Operation::Set {
            application: self.app.clone(),
            name: self.name.clone(),
            application_uid,
            requester: owner,
            role: self.role,
        };
        Request::new([1; 32], owner, operation.clone())?;
        Ok(operation)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = vec![
            1,
            match self.role {
                Role::Primary => 1,
                Role::Recovery => 2,
            },
        ];
        bytes.push(self.app.len() as u8);
        bytes.extend_from_slice(self.app.as_bytes());
        bytes.push(self.name.len() as u8);
        bytes.extend_from_slice(self.name.as_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let [1, role, app_len, rest @ ..] = bytes else {
            return Err("invalid write target".into());
        };
        let role = match role {
            1 => Role::Primary,
            2 => Role::Recovery,
            _ => return Err("invalid write token role".into()),
        };
        let (app, rest) = rest
            .split_at_checked(usize::from(*app_len))
            .ok_or("truncated write application")?;
        let (name_len, name) = rest.split_first().ok_or("missing write name")?;
        if name.len() != usize::from(*name_len) || bytes.len() > LIMIT {
            return Err("invalid write target length".into());
        }
        let app = std::str::from_utf8(app).map_err(|_| "invalid application encoding")?;
        let name = std::str::from_utf8(name).map_err(|_| "invalid name encoding")?;
        Self::parts(app, name, role)
    }
}

pub(crate) struct Credential(pub Vec<u8>);

pub(crate) fn require_protected_memory() -> Result<(), String> {
    match std::fs::read_to_string("/proc/swaps") {
        Ok(text)
            if text.lines().next().is_some()
                && !text.lines().skip(1).any(|line| !line.trim().is_empty()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        _ => return Err("credential input requires swap to be disabled".into()),
    }
    let limits = std::fs::read_to_string("/proc/self/limits").map_err(|e| e.to_string())?;
    let core = limits
        .lines()
        .find_map(|line| line.strip_prefix("Max core file size"));
    if core.and_then(|line| line.split_whitespace().next()) != Some("0") {
        return Err("credential input requires a zero core-dump soft limit".into());
    }
    Ok(())
}

impl Drop for Credential {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn public_targets_are_exact_canonical_typed_descriptions() {
        for role in [Role::Primary, Role::Recovery] {
            for target in [
                "mail/main".to_string(),
                format!("{}/{}", "a".repeat(64), "b".repeat(64)),
            ] {
                let parsed = Target::parse(&target, role).unwrap();
                let bytes = parsed.encode();
                assert!(bytes.len() <= LIMIT);
                assert_eq!(Target::decode(&bytes).unwrap(), parsed);
                for end in 0..bytes.len() {
                    assert!(Target::decode(&bytes[..end]).is_err());
                }
                let mut extra = bytes.clone();
                extra.push(0);
                assert!(Target::decode(&extra).is_err());
                let mut wrong = bytes;
                wrong[1] = 0;
                assert!(Target::decode(&wrong).is_err());
            }
        }
        for target in [
            "mail",
            "mail/",
            "/main",
            "mail/main/other",
            "Mail/main",
            "mail/../key",
            "mail/main\n",
        ] {
            assert!(Target::parse(target, Role::Primary).is_err());
        }
    }
}
