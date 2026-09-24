//! Checked scalar configuration values. No references, file trust or authority.
use super::syntax::Location;
use crate::format::row::MAX_ADDRESS;
use std::fmt;

pub const MAX_PROFILE_BYTES: usize = 64;
pub const MAX_PATH_BYTES: usize = 4095;
pub const MAX_DOMAIN_BYTES: usize = 243;
pub const MAX_CERTIFICATE_NAME_BYTES: usize = 253;
pub const MAX_LOCAL_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    ProfileName,
    DnsName,
    AbsolutePath,
    Mailbox,
}
impl Code {
    pub const fn name(self) -> &'static str {
        match self {
            Self::ProfileName => "config_value_profile_name",
            Self::DnsName => "config_value_dns_name",
            Self::AbsolutePath => "config_value_absolute_path",
            Self::Mailbox => "config_value_mailbox",
        }
    }
    /// The loader supplies the assignment value or labelled section location.
    pub const fn at(self, location: Location) -> Diagnostic {
        Diagnostic {
            code: self,
            location,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub code: Code,
    pub location: Location,
}
impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} at line {}, byte column {}",
            self.code.name(),
            self.location.line,
            self.location.column
        )
    }
}
impl std::error::Error for Diagnostic {}
impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
impl std::error::Error for Code {}

/// ASCII profile identifier; successful syntax validation grants no authority.
pub fn profile_name(input: &str) -> Result<(), Code> {
    if input.len() > MAX_PROFILE_BYTES
        || !input.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        || !input
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
    {
        return Err(Code::ProfileName);
    }
    Ok(())
}
/// Lexical validation only; protected descriptor opening belongs to M05.
/// The physical configuration parser separately rejects source controls.
/// This accepts root `/`, but never normalizes or resolves path components.
pub fn absolute_path(input: &str) -> Result<(), Code> {
    if input.len() > MAX_PATH_BYTES || input.bytes().any(|b| b == 0) {
        return Err(Code::AbsolutePath);
    }
    let tail = input.strip_prefix('/').ok_or(Code::AbsolutePath)?;
    if !tail.is_empty()
        && tail
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(Code::AbsolutePath);
    }
    Ok(())
}
/// Compare validated lexical roots by whole components, with equality included.
/// Symlink aliases and descriptor identity still require protected file checks.
pub fn paths_overlap(first: &str, second: &str) -> Result<bool, Code> {
    absolute_path(first)?;
    absolute_path(second)?;
    let ancestor = |parent: &str, child: &str| {
        parent == "/"
            || parent == child
            || child
                .strip_prefix(parent)
                .is_some_and(|tail| tail.starts_with('/'))
    };
    Ok(ancestor(first, second) || ancestor(second, first))
}
/// Validate a configured endpoint/routing name without DNS lookup or folding.
pub fn dns_name(input: &str) -> Result<(), Code> {
    if dns_valid(input, MAX_DOMAIN_BYTES) {
        Ok(())
    } else {
        Err(Code::DnsName)
    }
}
/// Validate a derived certificate name using its separate 253-byte ceiling.
pub fn certificate_name(input: &str) -> Result<(), Code> {
    if dns_valid(input, MAX_CERTIFICATE_NAME_BYTES) {
        Ok(())
    } else {
        Err(Code::DnsName)
    }
}
fn dns_valid(input: &str, maximum: usize) -> bool {
    !input.is_empty()
        && input.len() <= maximum
        && input.is_ascii()
        && input
            .rsplit('.')
            .next()
            .is_some_and(|label| !label.bytes().all(|b| b.is_ascii_digit()))
        && input.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}
fn atext(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'/'
                | b'='
                | b'?'
                | b'^'
                | b'_'
                | b'`'
                | b'{'
                | b'|'
                | b'}'
                | b'~'
        )
}
fn put(output: &mut [u8], length: &mut usize, byte: u8) -> Result<(), Code> {
    *output.get_mut(*length).ok_or(Code::Mailbox)? = byte;
    *length = length.checked_add(1).ok_or(Code::Mailbox)?;
    Ok(())
}
/// Internal canonical key: decoded local part, NUL separator, folded domain.
/// This is not a message-header or complete SMTP path parser. Local case is
/// preserved, including postmaster; inbound routing owns that exception.
/// Only the returned prefix is valid. Error may leave partial source bytes in
/// output; neither error nor success scrubs caller storage. No allocation.
pub fn mailbox_key<'a>(input: &str, output: &'a mut [u8; MAX_ADDRESS]) -> Result<&'a [u8], Code> {
    if input.len() > MAX_ADDRESS || !input.is_ascii() {
        return Err(Code::Mailbox);
    }
    let (local, domain) = input.rsplit_once('@').ok_or(Code::Mailbox)?;
    if !dns_valid(domain, MAX_DOMAIN_BYTES) || local.is_empty() || local.len() > MAX_LOCAL_BYTES {
        return Err(Code::Mailbox);
    }
    let mut length = 0;
    if let Some(body) = local.strip_prefix('"') {
        let body = body.strip_suffix('"').ok_or(Code::Mailbox)?;
        let mut bytes = body.bytes();
        while let Some(byte) = bytes.next() {
            let byte = if byte == b'\\' {
                let escaped = bytes.next().ok_or(Code::Mailbox)?;
                if !(32..=126).contains(&escaped) {
                    return Err(Code::Mailbox);
                }
                escaped
            } else {
                if !(32..=126).contains(&byte) || byte == b'"' {
                    return Err(Code::Mailbox);
                }
                byte
            };
            put(output, &mut length, byte)?;
        }
        if length == 0 {
            return Err(Code::Mailbox);
        }
    } else {
        if !local
            .split('.')
            .all(|atom| !atom.is_empty() && atom.bytes().all(atext))
        {
            return Err(Code::Mailbox);
        }
        for byte in local.bytes() {
            put(output, &mut length, byte)?;
        }
    }
    put(output, &mut length, 0)?;
    for byte in domain.bytes() {
        put(output, &mut length, byte.to_ascii_lowercase())?;
    }
    output.get(..length).ok_or(Code::Mailbox)
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    #[test]
    fn profile_alphabet_and_exact_byte_limit() {
        for good in ["a", "mail-2_tls", &"a".repeat(64)] {
            assert_eq!(profile_name(good), Ok(()));
        }
        for bad in [
            "",
            "A",
            "2a",
            "_a",
            "-a",
            "a.b",
            "a/b",
            "a b",
            "é",
            "a\0b",
            &"a".repeat(65),
        ] {
            assert_eq!(profile_name(bad), Err(Code::ProfileName), "{bad:?}");
        }
    }
    #[test]
    fn lexical_paths_preserve_bytes_and_reject_alias_components() {
        for good in [
            "/",
            "/var/lib/td-mta",
            "/é/a b",
            "/a\\b",
            &format!("/{}", "x".repeat(4094)),
        ] {
            assert_eq!(absolute_path(good), Ok(()));
        }
        for bad in [
            "",
            "a",
            "//",
            "/a/",
            "/a//b",
            "/./a",
            "/a/../b",
            "/a\0b",
            &format!("/{}", "x".repeat(4095)),
        ] {
            assert_eq!(absolute_path(bad), Err(Code::AbsolutePath), "{bad:?}");
        }
    }
    #[test]
    fn dns_shape_and_distinct_consumer_limits() {
        for good in [
            "a",
            "MX.Example.TEST",
            "a-0.test",
            "123.a",
            "xn--bcher-kva.test",
        ] {
            assert_eq!(dns_name(good), Ok(()));
        }
        for bad in [
            "",
            ".a",
            "a.",
            "a..b",
            "-a.test",
            "a-.test",
            "a_b.test",
            "a.123",
            "127.0.0.1",
            "[::1]",
            "a/b",
            "é.test",
            "a\0b",
            &format!("{}.test", "a".repeat(64)),
        ] {
            assert_eq!(dns_name(bad), Err(Code::DnsName), "{bad:?}");
        }
        for size in [243usize, 244, 253, 254] {
            let value = format!(
                "{}.{}.{}.{}",
                "a".repeat(63),
                "b".repeat(63),
                "c".repeat(63),
                "d".repeat(size - 192)
            );
            assert_eq!(value.len(), size);
            assert_eq!(dns_name(&value).is_ok(), size <= 243);
            let mut key = [0; MAX_ADDRESS];
            assert_eq!(
                mailbox_key(&format!("a@{value}"), &mut key).is_ok(),
                size <= 243
            );
            assert_eq!(
                certificate_name(&value),
                if size <= 253 {
                    Ok(())
                } else {
                    Err(Code::DnsName)
                }
            );
        }
    }
    #[test]
    fn mailbox_key_preserves_sender_local_case_and_quoted_equivalence() {
        let mut a = [0; MAX_ADDRESS];
        let mut b = [0; MAX_ADDRESS];
        assert_eq!(
            mailbox_key("PostMaster@EXAMPLE.TEST", &mut a).unwrap(),
            b"PostMaster\0example.test"
        );
        assert_ne!(
            mailbox_key("PostMaster@example.test", &mut a).unwrap(),
            mailbox_key("postmaster@example.test", &mut b).unwrap()
        );
        assert_eq!(
            mailbox_key(r#""U\ser"@EXAMPLE.TEST"#, &mut a).unwrap(),
            mailbox_key("User@example.test", &mut b).unwrap()
        );
        assert_eq!(mailbox_key(r#""a@b"@test"#, &mut a).unwrap(), b"a@b\0test");
        for bad in [
            "postmaster",
            "<a@test>",
            "@test",
            "a..b@test",
            "\"\"@test",
            "a@1",
            "a@[127.0.0.1]",
            "a@b.",
            "é@test",
        ] {
            assert_eq!(mailbox_key(bad, &mut a), Err(Code::Mailbox), "{bad:?}");
        }
    }
    #[test]
    fn path_roots_compare_whole_components_in_both_orders() {
        for (a, b, overlap) in [
            ("/", "/a", true),
            ("/a", "/a", true),
            ("/a", "/a/b", true),
            ("/a", "/a-other", false),
            ("/é", "/é/a", true),
            ("/é", "/éx", false),
            ("/a/b", "/a/c", false),
            ("/a", "/b/a", false),
        ] {
            assert_eq!(paths_overlap(a, b), Ok(overlap));
            assert_eq!(paths_overlap(b, a), Ok(overlap));
        }
        assert_eq!(paths_overlap("/a/", "/b"), Err(Code::AbsolutePath));
        assert_eq!(paths_overlap("/a", "b"), Err(Code::AbsolutePath));
    }
    #[test]
    fn diagnostics_keep_only_fixed_code_and_source_coordinates() {
        let at = Location {
            line: std::num::NonZeroU32::new(17).unwrap(),
            column: 29,
        };
        for (code, name) in [
            (Code::ProfileName, "config_value_profile_name"),
            (Code::DnsName, "config_value_dns_name"),
            (Code::AbsolutePath, "config_value_absolute_path"),
            (Code::Mailbox, "config_value_mailbox"),
        ] {
            assert_eq!(code.name(), name);
            assert_eq!(code.to_string(), name);
            let e = code.at(at);
            assert_eq!(e.code, code);
            assert_eq!(e.location, at);
            assert_eq!(e.to_string(), format!("{name} at line 17, byte column 29"));
            assert!(std::error::Error::source(&e).is_none());
        }
        let marker = "PRIVATE_source";
        let e = dns_name(marker).unwrap_err().at(at);
        assert!(!format!("{e:?} {e}").contains(marker));
    }
    #[test]
    fn serialized_address_limit_precedes_quote_contraction() {
        let local = format!("\"{}\"", "\\x".repeat(31));
        assert_eq!(local.len(), MAX_LOCAL_BYTES);
        let mut output = [0; MAX_ADDRESS];
        for (domain_bytes, accepted) in [(189usize, true), (190, false), (200, false)] {
            let domain = format!(
                "{}.{}.{}.{}",
                "a".repeat(60),
                "b".repeat(60),
                "c".repeat(60),
                "d".repeat(domain_bytes - 183)
            );
            assert_eq!(domain.len(), domain_bytes);
            assert_eq!(dns_name(&domain), Ok(()));
            let address = format!("{local}@{domain}");
            assert_eq!(address.len(), domain_bytes + 65);
            if accepted {
                let key = mailbox_key(&address, &mut output).unwrap();
                assert_eq!(key.len(), 31 + 1 + domain_bytes);
                assert_eq!(
                    key.get(..32).unwrap(),
                    format!("{}\0", "x".repeat(31)).as_bytes()
                );
            } else {
                assert_eq!(mailbox_key(&address, &mut output), Err(Code::Mailbox));
            }
        }
    }
}
