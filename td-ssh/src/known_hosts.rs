//! `known_hosts`: plain and `|1|` hashed names, patterns, `[host]:port` and
//! `@revoked` are read. Other marker and key types are skipped.

use crate::kex::ed25519_blob_key;
use crate::pattern::{match_list, ListMatch};
use td_encoding::{base64, from_base64};
use td_primitives::curve25519::ed25519;
use td_primitives::hmac;

const ALGORITHM: &str = "ssh-ed25519";
const HASH_MAGIC: &str = "|1|";

#[derive(Debug, PartialEq, Eq)]
pub enum Lookup {
    /// A recorded Ed25519 key matched.
    Match,
    /// `@revoked` names this key. Refused in every mode.
    Revoked,
    /// A different Ed25519 key is recorded. Refused in every mode.
    Mismatch,
    /// Nothing recorded for this host, in a key type td-ssh can compare.
    Unknown,
}

/// The name a host is recorded under: bare at port 22, else `[host]:port`.
pub fn record_name(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

/// What lookup and recording both use. `HostKeyAlias` is verbatim: it replaces
/// the name and the port, as OpenSSH's does.
pub fn host_key_name(alias: Option<&str>, host: &str, port: u16) -> String {
    match alias {
        Some(alias) => alias.to_string(),
        None => record_name(host, port),
    }
}

/// A file's lines that are text, numbered from one. One that is not UTF-8 is
/// skipped alone, so the pins around it still hold.
fn text_lines(bytes: &[u8]) -> impl Iterator<Item = (usize, &str)> {
    bytes
        .split(|byte| *byte == b'\n')
        .enumerate()
        .filter_map(|(index, line)| Some((index + 1, std::str::from_utf8(line).ok()?)))
}

/// Checks `key_blob_b64` against one file's bytes. Any revocation wins, then a
/// match, then a mismatch, so a stale line cannot mask a revocation.
pub fn lookup(text: impl AsRef<[u8]>, name: &str, key_b64: &str) -> Lookup {
    let mut matched = false;
    let mut mismatched = false;
    for (_, raw) in text_lines(text.as_ref()) {
        let Some(entry) = Entry::parse(raw) else {
            continue;
        };
        if !entry.names_host(name) {
            continue;
        }
        if entry.revoked {
            if entry.key == key_b64 {
                return Lookup::Revoked;
            }
            continue;
        }
        if entry.key == key_b64 {
            matched = true;
        } else {
            mismatched = true;
        }
    }
    if matched {
        Lookup::Match
    } else if mismatched {
        Lookup::Mismatch
    } else {
        Lookup::Unknown
    }
}

/// Where a `Mismatch` is no key at all: the first line naming this host with a
/// differing key, if no such line holds one that decodes. It is refused the same,
/// but "the key changed" would send a reader looking for the wrong thing.
pub fn corrupt_pin(text: impl AsRef<[u8]>, name: &str, key_b64: &str) -> Option<usize> {
    let mut first = None;
    for (line, raw) in text_lines(text.as_ref()) {
        let Some(entry) = Entry::parse(raw) else {
            continue;
        };
        if !entry.names_host(name) || entry.revoked || entry.key == key_b64 {
            continue;
        }
        let holds_a_key = from_base64(entry.key)
            .and_then(|blob| ed25519_blob_key(&blob).ok())
            .is_some();
        if holds_a_key {
            return None;
        }
        first = first.or(Some(line));
    }
    first
}

/// A plain entry for this host in a key type td-ssh cannot compare.
#[derive(Debug, PartialEq, Eq)]
pub struct OtherKey {
    pub line: usize,
    pub algorithm: String,
    pub key_b64: String,
}

/// Plain entries naming `name` in other key types. Marked lines are not pins of
/// a key the host would present, so they do not count.
pub fn other_keys(text: impl AsRef<[u8]>, name: &str) -> Vec<OtherKey> {
    let mut found = Vec::new();
    for (line, raw) in text_lines(text.as_ref()) {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('#') || raw.starts_with('@') {
            continue;
        }
        let mut fields = raw.split_ascii_whitespace();
        let (Some(hosts), Some(algorithm), Some(key)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if algorithm != ALGORITHM && names_host(hosts, name) {
            found.push(OtherKey {
                line,
                algorithm: algorithm.to_string(),
                key_b64: key.to_string(),
            });
        }
    }
    found
}

struct Entry<'a> {
    revoked: bool,
    hosts: &'a str,
    key: &'a str,
}

impl<'a> Entry<'a> {
    fn parse(line: &'a str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let mut fields = line.split_ascii_whitespace();
        let mut first = fields.next()?;
        let mut revoked = false;
        if let Some(marker) = first.strip_prefix('@') {
            match marker {
                "revoked" => revoked = true,
                // A CA line vouches for certificates, which td-ssh has no code for.
                _ => return None,
            }
            first = fields.next()?;
        }
        if fields.next()? != ALGORITHM {
            return None;
        }
        let key = fields.next()?;
        if holds_small_order_key(key) {
            return None;
        }
        Some(Self {
            revoked,
            hosts: first,
            key,
        })
    }

    fn names_host(&self, name: &str) -> bool {
        names_host(self.hosts, name)
    }
}

/// Pins compare as text, so this is the one reason to decode a recorded key: a
/// pin on a key of small order pins nothing (DESIGN.md section 3). What does not
/// decode stays an ordinary pin that no presented key can equal.
fn holds_small_order_key(key_b64: &str) -> bool {
    from_base64(key_b64)
        .and_then(|blob| ed25519_blob_key(&blob).ok())
        .is_some_and(|key| ed25519::is_small_order(&key))
}

fn names_host(hosts: &str, name: &str) -> bool {
    if hosts.starts_with(HASH_MAGIC) {
        hashed_matches(hosts, name)
    } else {
        match_list(hosts, name) == ListMatch::Accepted
    }
}

fn hashed_matches(field: &str, name: &str) -> bool {
    let Some(rest) = field.strip_prefix(HASH_MAGIC) else {
        return false;
    };
    let Some((salt, digest)) = rest.split_once('|') else {
        return false;
    };
    let (Some(salt), Some(digest)) = (from_base64(salt), from_base64(digest)) else {
        return false;
    };
    salt.len() == 20 && td_primitives::ct::eq(&hmac::hmac_sha1(&salt, name.as_bytes()), &digest)
}

/// The line to append for a newly accepted key.
pub fn new_line(name: &str, key_b64: &str, hash: Option<&[u8; 20]>) -> String {
    match hash {
        Some(salt) => {
            let digest = hmac::hmac_sha1(salt, name.as_bytes());
            format!(
                "{HASH_MAGIC}{}|{} {ALGORITHM} {key_b64}\n",
                base64(salt),
                base64(&digest)
            )
        }
        None => format!("{name} {ALGORITHM} {key_b64}\n"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const KEY: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIGood";
    const OTHER: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIBad0";

    #[test]
    fn plain_patterns_and_ports() {
        let text = format!("host.example,10.0.0.1 ssh-ed25519 {KEY}\n*.wild.example,!no.wild.example ssh-ed25519 {KEY}\n[alt.example]:2222 ssh-ed25519 {KEY}\n");
        assert_eq!(lookup(&text, "host.example", KEY), Lookup::Match);
        assert_eq!(lookup(&text, "10.0.0.1", KEY), Lookup::Match);
        assert_eq!(lookup(&text, "a.wild.example", KEY), Lookup::Match);
        assert_eq!(lookup(&text, "no.wild.example", KEY), Lookup::Unknown);
        assert_eq!(lookup(&text, "elsewhere", KEY), Lookup::Unknown);
        assert_eq!(lookup(&text, "[alt.example]:2222", KEY), Lookup::Match);
        assert_eq!(lookup(&text, "alt.example", KEY), Lookup::Unknown);
    }

    #[test]
    fn a_changed_key_is_a_mismatch_and_any_match_wins_over_it() {
        let text = format!("h ssh-ed25519 {OTHER}\n");
        assert_eq!(lookup(&text, "h", KEY), Lookup::Mismatch);
        let both = format!("h ssh-ed25519 {OTHER}\nh ssh-ed25519 {KEY}\n");
        assert_eq!(lookup(&both, "h", KEY), Lookup::Match);
    }

    #[test]
    fn a_pin_that_does_not_decode_is_told_from_a_key_that_changed() {
        let real = base64(&crate::kex::ed25519_public_blob(&[7; 32]));
        let other = base64(&crate::kex::ed25519_public_blob(&[8; 32]));
        let corrupt = "# a comment\nh ssh-ed25519 AAAAbroken\n";
        assert_eq!(lookup(corrupt, "h", &real), Lookup::Mismatch);
        assert_eq!(corrupt_pin(corrupt, "h", &real), Some(2));
        assert_eq!(corrupt_pin(corrupt, "elsewhere", &real), None);
        // A key that really differs is the serious reading, whatever is beside it.
        let changed = format!("h ssh-ed25519 AAAAbroken\nh ssh-ed25519 {other}\n");
        assert_eq!(corrupt_pin(&changed, "h", &real), None);
        let changed = format!("h ssh-ed25519 {other}\n");
        assert_eq!(corrupt_pin(&changed, "h", &real), None);
    }

    #[test]
    fn revocation_beats_a_match_in_any_order() {
        for text in [
            format!("h ssh-ed25519 {KEY}\n@revoked h ssh-ed25519 {KEY}\n"),
            format!("@revoked h ssh-ed25519 {KEY}\nh ssh-ed25519 {KEY}\n"),
        ] {
            assert_eq!(lookup(&text, "h", KEY), Lookup::Revoked);
        }
        // Revoking some other key does not touch this one.
        let text = format!("@revoked h ssh-ed25519 {OTHER}\nh ssh-ed25519 {KEY}\n");
        assert_eq!(lookup(&text, "h", KEY), Lookup::Match);
    }

    #[test]
    fn other_types_and_markers_and_junk_are_skipped() {
        let text = format!(
            "# comment\n\n@cert-authority h ssh-ed25519 {KEY}\nh ssh-rsa {OTHER}\nh\nh ssh-ed25519\n"
        );
        assert_eq!(lookup(&text, "h", KEY), Lookup::Unknown);
    }

    #[test]
    fn hashed_entries_round_trip_and_match_only_their_name() {
        let line = new_line("h.example", KEY, Some(&[7; 20]));
        assert!(line.starts_with("|1|"));
        assert_eq!(lookup(&line, "h.example", KEY), Lookup::Match);
        assert_eq!(lookup(&line, "other.example", KEY), Lookup::Unknown);
        assert_eq!(lookup(&line, "h.example", OTHER), Lookup::Mismatch);
        assert_eq!(new_line("h", KEY, None), format!("h ssh-ed25519 {KEY}\n"));
    }

    #[test]
    fn a_known_hashed_line_from_ssh_keygen_format_matches() {
        // |1|salt|hash for "github.com" from HMAC-SHA1(salt, name), salt 20 x 0x01.
        let line = new_line("github.com", KEY, Some(&[1; 20]));
        assert!(line.starts_with("|1|AQEBAQEBAQEBAQEBAQEBAQEBAQE=|"));
        assert_eq!(lookup(&line, "github.com", KEY), Lookup::Match);
    }

    #[test]
    fn malformed_hashes_do_not_match_or_panic() {
        for hosts in ["|1|", "|1|x", "|1|!!|!!", "|1|Zm9v|Zm9v", "|2|a|b"] {
            let text = format!("{hosts} ssh-ed25519 {KEY}\n");
            assert_eq!(lookup(&text, "h", KEY), Lookup::Unknown, "{hosts}");
        }
    }

    #[test]
    fn record_names_follow_openssh() {
        assert_eq!(record_name("h", 22), "h");
        assert_eq!(record_name("h", 2222), "[h]:2222");
    }

    #[test]
    fn an_alias_is_verbatim_at_any_port() {
        assert_eq!(host_key_name(Some("td-host"), "h", 2222), "td-host");
        assert_eq!(host_key_name(Some("Td-Host"), "h", 22), "Td-Host");
        assert_eq!(host_key_name(None, "h", 2222), "[h]:2222");
        let text = format!("td-host ssh-ed25519 {KEY}\n");
        let name = host_key_name(Some("td-host"), "10.0.0.1", 2222);
        assert_eq!(lookup(&text, &name, KEY), Lookup::Match);
    }

    #[test]
    fn a_line_that_is_not_text_is_skipped_alone() {
        let mut bytes = b"\xff\xfe not text\n".to_vec();
        bytes.extend_from_slice(format!("h ssh-ed25519 {OTHER}\n").as_bytes());
        bytes.extend_from_slice(b"h ssh-ed25519 \xff\n");
        assert_eq!(lookup(&bytes, "h", KEY), Lookup::Mismatch);
        assert_eq!(lookup(&bytes, "h", OTHER), Lookup::Match);
    }

    #[test]
    fn a_line_holding_a_key_of_small_order_is_unusable() {
        let mut identity = [0u8; 32];
        identity[0] = 1;
        let small = base64(&crate::kex::ed25519_public_blob(&identity));
        let ordinary = base64(&crate::kex::ed25519_public_blob(&[3; 32]));
        // It neither matches nor stands as a pin another key would mismatch.
        let text = format!("h ssh-ed25519 {small}\n");
        assert_eq!(lookup(&text, "h", &small), Lookup::Unknown);
        assert_eq!(lookup(&text, "h", &ordinary), Lookup::Unknown);
        // The lines around it still hold.
        let both = format!("h ssh-ed25519 {small}\nh ssh-ed25519 {ordinary}\n");
        assert_eq!(lookup(&both, "h", &ordinary), Lookup::Match);
        assert_eq!(lookup(&both, "h", KEY), Lookup::Mismatch);
        let revoked = format!("@revoked h ssh-ed25519 {small}\n");
        assert_eq!(lookup(&revoked, "h", &small), Lookup::Unknown);
    }

    #[test]
    fn other_key_types_are_found_with_their_line() {
        let hashed = new_line("h", KEY, Some(&[3; 20])).replace(ALGORITHM, "ssh-rsa");
        let text = format!(
            "# c\nh ssh-ed25519 {KEY}\nh,i ecdsa-sha2-nistp256 AAAA\nj ssh-rsa BBBB\n@revoked h ssh-rsa CCCC\n{hashed}"
        );
        let found = other_keys(&text, "h");
        assert_eq!(
            found,
            [
                OtherKey {
                    line: 3,
                    algorithm: "ecdsa-sha2-nistp256".into(),
                    key_b64: "AAAA".into()
                },
                OtherKey {
                    line: 6,
                    algorithm: "ssh-rsa".into(),
                    key_b64: KEY.into()
                }
            ]
        );
        assert!(other_keys(&text, "absent").is_empty());
    }
}
