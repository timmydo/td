//! Shared policy/profile partitioning before whole-profile validation.
use super::{stanza::Section, stream, syntax};
use std::io::{self, Read};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Part {
    Shared,
    Profile,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    Capacity,
    Root,
    Scope,
    Syntax(syntax::Diagnostic),
    Stream,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error {
    pub part: Part,
    pub code: Code,
}

fn belongs(section: Section, part: Part) -> bool {
    match section {
        Section::Account
        | Section::Domain
        | Section::Alias
        | Section::Identity
        | Section::IdentityAddress => part == Part::Shared,
        Section::Server
        | Section::Resolver
        | Section::Logging
        | Section::Relay
        | Section::Certificate
        | Section::Acme
        | Section::Gateway
        | Section::GatewayPeer
        | Section::Listener
        | Section::Resource(_) => part == Part::Profile,
        // Host-local placement remains bootstrap input, never database policy.
        Section::Paths => false,
    }
}

fn check(mut reader: impl Read, part: Part, scratch: &mut [u8]) -> Result<(), Error> {
    let mut root = true;
    let mut version = false;
    stream::read(&mut reader, scratch, |statement| {
        match statement {
            syntax::Statement::Empty => {}
            syntax::Statement::Section { name, .. } => {
                if part == Part::Shared && !version {
                    return Err(Code::Root);
                }
                let section = Section::parse(name).ok_or(Code::Scope)?;
                if !belongs(section, part) {
                    return Err(Code::Scope);
                }
                root = false;
            }
            syntax::Statement::Assignment { key, value, .. } if root => {
                if part != Part::Shared
                    || version
                    || key != "version"
                    || value != syntax::Value::Integer(1)
                {
                    return Err(Code::Root);
                }
                version = true;
            }
            syntax::Statement::Assignment { .. } => {}
        }
        Ok(())
    })
    .map_err(|error| Error {
        part,
        code: match error {
            stream::Error::Handler(code) => code,
            stream::Error::Syntax(diagnostic) => Code::Syntax(diagnostic),
            stream::Error::Capacity => Code::Capacity,
            _ => Code::Stream,
        },
    })?;
    if (part == Part::Shared && !version) || (part == Part::Profile && root) {
        return Err(Error {
            part,
            code: Code::Root,
        });
    }
    Ok(())
}

/// Borrowed, scope-checked syntax only. Field/reference/content checks and
/// generation-wide admission remain mandatory. No owned bytes are copied.
pub struct Source<'a> {
    shared: &'a [u8],
    profile: &'a [u8],
}
impl<'a> Source<'a> {
    pub fn new(shared: &'a [u8], profile: &'a [u8], scratch: &mut [u8]) -> Result<Self, Error> {
        let total = shared
            .len()
            .checked_add(profile.len())
            .and_then(|n| n.checked_add(1));
        if total.is_none_or(|n| n > syntax::MAX_INPUT_BYTES) {
            return Err(Error {
                part: Part::Profile,
                code: Code::Capacity,
            });
        }
        check(shared.chain(b"\n".as_slice()), Part::Shared, scratch)?;
        check(profile, Part::Profile, scratch)?;
        Ok(Self { shared, profile })
    }

    /// Feed this reader through load::read_profile to EOF. The inserted LF
    /// terminates a shared final line/comment before the first profile section.
    /// That final loader also enforces aggregate physical-line limits.
    pub fn reader(self) -> impl io::Read + 'a {
        self.shared.chain(b"\n".as_slice()).chain(self.profile)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    #[test]
    fn partitions_cannot_override_policy_or_bootstrap_and_bound_combined_bytes() {
        use super::*;
        let mut scratch = vec![0; stream::SCRATCH_BYTES];
        for (shared, profile, part, code) in [
            ("", "[server]", Part::Shared, Code::Root),
            ("version = 2", "[server]", Part::Shared, Code::Root),
            ("[account \"a\"]", "[server]", Part::Shared, Code::Root),
            ("version = 1", "version = 1", Part::Profile, Code::Root),
            ("version = 1", "hostname = \"x\"", Part::Profile, Code::Root),
            ("version = 1", "[account \"a\"]", Part::Profile, Code::Scope),
            (
                "version = 1\n[listener \"x\"]",
                "[server]",
                Part::Shared,
                Code::Scope,
            ),
            (
                "version = 1\n[paths]",
                "[server]",
                Part::Shared,
                Code::Scope,
            ),
            ("version = 1", "[paths]", Part::Profile, Code::Scope),
            ("version = 1", "[unknown]", Part::Profile, Code::Scope),
        ] {
            assert_eq!(
                Source::new(shared.as_bytes(), profile.as_bytes(), &mut scratch).err(),
                Some(Error { part, code })
            );
        }
        let full_line = format!("version = 1\n#{}", "x".repeat(syntax::MAX_LINE_BYTES - 1));
        assert!(
            matches!(Source::new(full_line.as_bytes(), b"[server]", &mut scratch).err(), Some(Error { part: Part::Shared, code: Code::Syntax(diagnostic) }) if diagnostic.code == syntax::Code::LineTooLong)
        );
        let fitting_line = format!("version = 1\n#{}", "x".repeat(syntax::MAX_LINE_BYTES - 2));
        assert!(Source::new(fitting_line.as_bytes(), b"[server]", &mut scratch).is_ok());
        let oversized = vec![b' '; syntax::MAX_INPUT_BYTES];
        assert_eq!(
            Source::new(&oversized, b"[server]", &mut scratch).err(),
            Some(Error {
                part: Part::Profile,
                code: Code::Capacity
            })
        );
        let source = Source::new(
            b"version = 1 # final comment",
            b"[server]\nhostname = \"x\"",
            &mut scratch,
        )
        .unwrap();
        let mut composed = Vec::new();
        source.reader().read_to_end(&mut composed).unwrap();
        assert_eq!(
            composed,
            b"version = 1 # final comment\n[server]\nhostname = \"x\""
        );
    }

    #[test]
    fn one_shared_policy_loads_receiving_jmap_and_relay_profiles() {
        use super::*;
        use crate::config::{load, profile::Roles, stanza::Pending, storage::Storage, text};
        let _serial = text::TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let shared = br#"version = 1
[account "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
username = "user"
[domain "example.test"]
mx_host = "mail.example.test"
[alias "main@example.test"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[identity "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
email = "main@example.test""#;
        let base = r#"[server]
hostname = "mail.example.test"
jmap_origin = "https://jmap.example.test"
[resolver "primary"]
address = "127.0.0.1:53"
[relay]
host = "relay.example.test"
port = 465
username = "relay-user"
password_file = "/postgres-material/01010101010101010101010101010101"
"#;
        let cert = r#"[certificate "public"]
mode = "files"
chain_file = "/postgres-material/02020202020202020202020202020202"
key_file = "/postgres-material/03030303030303030303030303030303"
"#;
        let smtp = r#"[listener "smtp"]
kind = "direct_smtp"
bind = "0.0.0.0:25"
server_name = "mail.example.test"
certificate = "public"
session_limit = 1
per_peer_limit = 1
"#;
        let https = r#"[listener "https"]
kind = "https"
bind = "127.0.0.1:443"
certificate = "public"
"#;
        let mut scratch = vec![0; stream::SCRATCH_BYTES];
        let mut storage = Storage::try_new().unwrap();
        for bits in 1..=7 {
            let roles = Roles::from_bits(bits).unwrap();
            let mut profile = base.to_owned();
            if roles.receive() || roles.jmap() {
                profile.push_str(cert);
            }
            if roles.receive() {
                profile.push_str(smtp);
            }
            if roles.jmap() {
                profile.push_str(https);
            }
            let source = Source::new(shared, profile.as_bytes(), &mut scratch).unwrap();
            let loaded = load::read_profile(
                storage,
                &mut Pending::new(),
                &mut scratch,
                &mut source.reader(),
                roles,
            )
            .unwrap();
            assert_eq!(loaded.roles(), roles);
            assert!(loaded
                .candidate()
                .identities()
                .unwrap()
                .identity(0)
                .unwrap()
                .is_some());
            storage = loaded.into_storage();
        }
    }
}
