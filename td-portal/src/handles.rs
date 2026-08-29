//! Bounded caller-owned Request and Session handle objects.

use std::collections::BTreeMap;

pub const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
pub const SESSION_INTERFACE: &str = "org.freedesktop.portal.Session";
pub const MAX_HANDLES: usize = 64;
pub const MAX_HANDLES_PER_OWNER: usize = 16;
pub const MAX_TOKEN_BYTES: usize = 64;

const REQUEST_PREFIX: &str = "/org/freedesktop/portal/desktop/request";
const SESSION_PREFIX: &str = "/org/freedesktop/portal/desktop/session";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandleKind {
    Request,
    Session,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ReserveError {
    InvalidOwner,
    InvalidToken,
    Duplicate,
    OwnerFull,
    Full,
}

#[derive(Debug, Eq, PartialEq)]
pub enum LookupError {
    Missing,
    Foreign,
}

#[derive(Debug)]
struct Handle {
    owner: String,
    kind: HandleKind,
}

#[derive(Debug, Default)]
pub struct Handles {
    entries: BTreeMap<String, Handle>,
}

impl Handles {
    pub fn reserve(
        &mut self,
        kind: HandleKind,
        owner: &str,
        token: &str,
    ) -> Result<String, ReserveError> {
        let owner_element = owner_element(owner).ok_or(ReserveError::InvalidOwner)?;
        if !valid_token(token) {
            return Err(ReserveError::InvalidToken);
        }
        let prefix = match kind {
            HandleKind::Request => REQUEST_PREFIX,
            HandleKind::Session => SESSION_PREFIX,
        };
        let path = format!("{prefix}/{owner_element}/{token}");
        if self.entries.contains_key(&path) {
            return Err(ReserveError::Duplicate);
        }
        if self
            .entries
            .values()
            .filter(|handle| handle.owner == owner)
            .count()
            >= MAX_HANDLES_PER_OWNER
        {
            return Err(ReserveError::OwnerFull);
        }
        if self.entries.len() >= MAX_HANDLES {
            return Err(ReserveError::Full);
        }
        self.entries.insert(
            path.clone(),
            Handle {
                owner: owner.to_string(),
                kind,
            },
        );
        Ok(path)
    }

    pub fn lookup(&self, path: &str, owner: &str) -> Result<HandleKind, LookupError> {
        let handle = self.entries.get(path).ok_or(LookupError::Missing)?;
        if handle.owner != owner {
            return Err(LookupError::Foreign);
        }
        Ok(handle.kind)
    }

    pub fn retire(&mut self, path: &str) -> bool {
        self.entries.remove(path).is_some()
    }

    pub fn remove_owner(&mut self, owner: &str) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, handle| handle.owner != owner);
        before.saturating_sub(self.entries.len())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

pub fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_TOKEN_BYTES
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

pub fn request_path(owner: &str, token: &str) -> Option<String> {
    let owner = owner_element(owner)?;
    valid_token(token).then(|| format!("{REQUEST_PREFIX}/{owner}/{token}"))
}

fn owner_element(owner: &str) -> Option<String> {
    let sequence = owner.strip_prefix(":1.")?;
    if sequence.is_empty() || !sequence.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number = sequence.parse::<u64>().ok()?;
    if number.to_string() != sequence {
        return None;
    }
    Some(format!("1_{sequence}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn paths_are_caller_derived_and_tokens_are_closed() {
        assert_eq!(
            request_path(":1.42", "t7").as_deref(),
            Some("/org/freedesktop/portal/desktop/request/1_42/t7")
        );
        for bad in ["", "a-b", "a/b", "x.y", &"x".repeat(MAX_TOKEN_BYTES + 1)] {
            assert!(!valid_token(bad), "accepted {bad:?}");
        }
        for bad in [":1_42", ":1.04", ":2.4", ":1.4.2", ":1.a"] {
            assert!(request_path(bad, "t7").is_none(), "accepted {bad:?}");
        }
    }

    #[test]
    fn ownership_capacity_retirement_and_departure_are_exact() {
        let mut handles = Handles::default();
        let request = handles
            .reserve(HandleKind::Request, ":1.2", "request")
            .unwrap();
        let session = handles
            .reserve(HandleKind::Session, ":1.2", "session")
            .unwrap();
        assert_eq!(handles.lookup(&request, ":1.2"), Ok(HandleKind::Request));
        assert_eq!(handles.lookup(&request, ":1.3"), Err(LookupError::Foreign));
        assert_eq!(
            handles.reserve(HandleKind::Request, ":1.2", "request"),
            Err(ReserveError::Duplicate)
        );
        assert!(handles.retire(&request));
        assert!(!handles.retire(&request));
        assert_eq!(handles.remove_owner(":1.2"), 1);
        assert_eq!(handles.lookup(&session, ":1.2"), Err(LookupError::Missing));

        for serial in 0..MAX_HANDLES_PER_OWNER {
            handles
                .reserve(HandleKind::Request, ":1.4", &format!("t{serial}"))
                .unwrap();
        }
        assert_eq!(
            handles.reserve(HandleKind::Session, ":1.4", "overflow"),
            Err(ReserveError::OwnerFull)
        );
        assert!(handles
            .reserve(HandleKind::Session, ":1.5", "other_owner")
            .is_ok());

        let mut global = Handles::default();
        for owner in 10..14 {
            for serial in 0..MAX_HANDLES_PER_OWNER {
                global
                    .reserve(
                        HandleKind::Request,
                        &format!(":1.{owner}"),
                        &format!("t{serial}"),
                    )
                    .unwrap();
            }
        }
        assert_eq!(global.len(), MAX_HANDLES);
        assert_eq!(
            global.reserve(HandleKind::Session, ":1.20", "overflow"),
            Err(ReserveError::Full)
        );
    }
}
