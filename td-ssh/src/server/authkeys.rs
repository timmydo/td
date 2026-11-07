//! `authorized_keys` lines and the files that hold them. An option this code
//! does not understand makes the line unusable, never unrestricted (DESIGN.md
//! section 5).

use crate::keyfile::PublicKey;
use crate::pattern::{match_list, ListMatch};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Read};
use std::net::IpAddr;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub const MAX_KEY_FILE: u64 = 256 * 1024;
const O_NONBLOCK: i32 = 0o4000;
const GROUP_OTHER_WRITE: u32 = 0o022;
const STICKY: u32 = 0o1000;

/// Why a key file counts as empty. Missing is ordinary and logged by nobody.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyFileError {
    Missing,
    Refused(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Grant {
    pub no_pty: bool,
    pub command: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Line {
    key: Option<[u8; 32]>,
    grant: Grant,
    from: Option<String>,
}

/// Splits on commas outside double quotes, as an options field is written.
fn split_options(text: &str) -> Option<Vec<String>> {
    let mut options = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' if in_quotes => {
                let next = chars.next()?;
                if next != '"' {
                    current.push('\\');
                }
                current.push(next);
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => options.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    if in_quotes {
        return None;
    }
    options.push(current);
    Some(options)
}

/// The end of an options field: the first unquoted space or tab.
fn options_end(line: &str) -> Option<usize> {
    let mut in_quotes = false;
    let mut escaped = false;
    for (index, c) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            ' ' | '\t' if !in_quotes => return Some(index),
            _ => {}
        }
    }
    None
}

fn parse_line(raw: &str) -> Option<Line> {
    let line = raw.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut parsed = Line::default();
    let key_text = if line.starts_with("ssh-ed25519 ") || line.starts_with("ssh-ed25519\t") {
        line
    } else {
        let end = options_end(line)?;
        let (options, rest) = line.split_at(end);
        let options = split_options(options)?;
        // Last-wins would let a second `from=` undo the first.
        let mut names = HashSet::new();
        for option in &options {
            if !names.insert(
                option
                    .split_once('=')
                    .map_or(option.as_str(), |pair| pair.0),
            ) {
                return None;
            }
        }
        for option in options {
            match option.split_once('=') {
                None => match option.as_str() {
                    "restrict" | "no-pty" => parsed.grant.no_pty = true,
                    // Nothing here could ever have been granted: forwarding,
                    // agents and user rc have no code in this server.
                    "no-port-forwarding"
                    | "no-agent-forwarding"
                    | "no-X11-forwarding"
                    | "no-user-rc" => {}
                    _ => return None,
                },
                Some(("from", value)) if valid_from(value) => {
                    parsed.from = Some(value.to_string());
                }
                Some(("command", value)) => parsed.grant.command = Some(value.to_string()),
                Some(_) => return None,
            }
        }
        rest.trim_start()
    };
    parsed.key = Some(PublicKey::parse_line(key_text).ok()?.key);
    Some(parsed)
}

/// One `from=` entry, which must be able to match the one form a peer's address
/// is ever printed in. A negated entry that cannot match widens access, so
/// anything that cannot is refused here and not left to fail to match: a CIDR, a
/// hostname (there is no DNS), a mapped or non-canonical literal, an impossible
/// octet. Without a colon an entry is IPv4, so `cafe.de` is no address. A negated
/// IPv6 glob is refused too, since compression makes its match unknowable.
fn valid_from_entry(entry: &str) -> bool {
    let (negated, pattern) = match entry.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, entry),
    };
    let is_v6 = pattern.contains(':');
    let shape = !pattern.is_empty()
        && pattern.bytes().all(|byte| {
            matches!(byte, b'0'..=b'9' | b'.' | b'*' | b'?')
                || (is_v6 && (byte == b':' || byte.is_ascii_hexdigit()))
        });
    if !shape {
        return false;
    }
    if !pattern.contains(['*', '?']) {
        return pattern.parse::<IpAddr>().is_ok_and(|address| {
            address.to_canonical() == address && address.to_string() == pattern.to_ascii_lowercase()
        });
    }
    // A pattern that can never match excludes nobody, and the entry after it then
    // admits everyone: the one way to read a negation that fails open.
    if negated {
        return false;
    }
    if is_v6 {
        return true;
    }
    pattern.split('.').count() <= 4
        && pattern.split('.').all(|group| {
            group.contains(['*', '?'])
                || (group.parse::<u8>().is_ok() && (group == "0" || !group.starts_with('0')))
        })
}

fn valid_from(list: &str) -> bool {
    list.split(',').all(valid_from_entry)
}

/// `from=`: exact addresses, `*`/`?` patterns and `!` negation over the peer's
/// address, which the monitor has already normalized out of v4-mapped form.
fn from_allows(pattern: &str, peer: &str) -> bool {
    match_list(pattern, peer) == ListMatch::Accepted
}

/// The first line of `text` that names `key` for a peer at `peer`. A line whose
/// `from=` refuses this peer does not end the search, and a later unrestricted
/// line for the same key grants as written, as in OpenSSH. A line that is not
/// UTF-8 is one bad line, not a bad file.
pub fn find(text: impl AsRef<[u8]>, key: &[u8; 32], peer: &str) -> Option<Grant> {
    text.as_ref()
        .split(|byte| *byte == b'\n')
        .filter_map(|raw| std::str::from_utf8(raw).ok())
        .filter_map(parse_line)
        .find(|line| {
            line.key.as_ref() == Some(key)
                && line
                    .from
                    .as_deref()
                    .is_none_or(|pattern| from_allows(pattern, peer))
        })
        .map(|line| line.grant)
}

/// A directory step of strict modes. A sticky directory protects a chain with a
/// non-root `owner`, because nobody else can replace an entry there that they do
/// not own and the walk has already judged the entry below it. It does not
/// protect a root-only chain (`owner` 0: the absolute form and the real daemon's
/// host key): anyone can plant a missing entry or a symlink in `/tmp`, and root
/// would read it. A test sandbox's `/` and `/tmp` are 1777 and owned by the test's
/// uid, and `--single-user` must run there.
fn directory_passes(uid: u32, mode: u32, owner: u32) -> bool {
    let sticky_counts = owner != 0 && mode & STICKY != 0;
    (uid == 0 || uid == owner) && (mode & GROUP_OTHER_WRITE == 0 || sticky_counts)
}

/// Strict modes on a resolved path: the file and every directory above it are
/// owned by root or `owner` and writable by nobody else. Pass 0 for root alone.
/// `directory_passes` says what a sticky directory counts for.
pub fn check_path(resolved: &Path, owner: u32) -> Result<(), String> {
    let metadata = fs::symlink_metadata(resolved)
        .map_err(|error| format!("{}: {error}", resolved.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", resolved.display()));
    }
    let owner_fine = |uid: u32| uid == 0 || uid == owner;
    if !owner_fine(metadata.uid()) || metadata.mode() & GROUP_OTHER_WRITE != 0 {
        return Err(format!(
            "bad ownership or modes for file {}",
            resolved.display()
        ));
    }
    let mut current = resolved.parent();
    while let Some(directory) = current {
        let metadata = fs::symlink_metadata(directory)
            .map_err(|error| format!("{}: {error}", directory.display()))?;
        if !directory_passes(metadata.uid(), metadata.mode(), owner) {
            return Err(format!(
                "bad ownership or modes for directory {}",
                directory.display()
            ));
        }
        current = directory.parent();
    }
    Ok(())
}

fn located(path: &Path, error: &io::Error) -> KeyFileError {
    if error.kind() == io::ErrorKind::NotFound {
        KeyFileError::Missing
    } else {
        KeyFileError::Refused(format!("{}: {error}", path.display()))
    }
}

/// Reads a key file after checking it, resolving symlinks first so reviewed
/// links such as `/etc/ssh/authorized_keys` are judged by where they lead. A
/// FIFO cannot stall this: the open does not block and the type is checked on
/// the descriptor.
pub fn read_checked(path: &Path, owner: u32) -> Result<Vec<u8>, KeyFileError> {
    let resolved: PathBuf = fs::canonicalize(path).map_err(|error| located(path, &error))?;
    check_path(&resolved, owner).map_err(KeyFileError::Refused)?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(&resolved)
        .map_err(|error| located(&resolved, &error))?;
    let metadata = file
        .metadata()
        .map_err(|error| located(&resolved, &error))?;
    if !metadata.is_file() || metadata.len() > MAX_KEY_FILE {
        return Err(KeyFileError::Refused(format!(
            "{} is not a regular file of acceptable size",
            resolved.display()
        )));
    }
    let mut bytes = Vec::new();
    Read::take(&mut file, MAX_KEY_FILE)
        .read_to_end(&mut bytes)
        .map_err(|error| located(&resolved, &error))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn key_line(key: &[u8; 32]) -> String {
        PublicKey {
            key: *key,
            comment: "c".into(),
        }
        .to_line()
    }

    const KEY: [u8; 32] = [4; 32];
    const OTHER: [u8; 32] = [5; 32];

    #[test]
    fn plain_lines_grant_and_other_keys_do_not() {
        let text = key_line(&KEY);
        assert_eq!(find(&text, &KEY, "10.0.0.1"), Some(Grant::default()));
        assert_eq!(find(&text, &OTHER, "10.0.0.1"), None);
    }

    #[test]
    fn restrict_and_from_as_the_boot_self_test_writes_them() {
        let text = format!("restrict,from=\"127.0.0.1\" {}", key_line(&KEY));
        let grant = find(&text, &KEY, "127.0.0.1").unwrap();
        assert!(grant.no_pty);
        assert_eq!(find(&text, &KEY, "10.0.0.1"), None);
    }

    #[test]
    fn command_is_unescaped_and_quoted_spaces_do_not_end_the_options() {
        let text = format!("command=\"echo \\\"hi there\\\"\" {}", key_line(&KEY));
        assert_eq!(
            find(&text, &KEY, "x").unwrap().command.as_deref(),
            Some("echo \"hi there\"")
        );
    }

    #[test]
    fn an_unknown_option_or_key_type_makes_the_line_unusable_never_unrestricted() {
        for options in [
            "environment=\"A=b\"",
            "tunnel=\"0\"",
            "cert-authority",
            "bogus",
        ] {
            let text = format!("{options} {}", key_line(&KEY));
            assert_eq!(find(&text, &KEY, "x"), None, "{options}");
        }
        assert_eq!(find("ssh-rsa AAAA c\n", &KEY, "x"), None);
        // An unterminated quote never parses as options plus a key.
        let text = format!("from=\"unterminated {}", key_line(&KEY));
        assert_eq!(find(&text, &KEY, "x"), None);
    }

    #[test]
    fn from_patterns_negate_and_never_match_a_name_with_no_dns() {
        let line = |pattern: &str| format!("from=\"{pattern}\" {}", key_line(&KEY));
        assert!(find(line("10.0.0.*"), &KEY, "10.0.0.7").is_some());
        assert!(find(line("10.0.0.*,!10.0.0.7"), &KEY, "10.0.0.7").is_none());
        assert!(find(line("10.0.0.*,!10.0.0.7"), &KEY, "10.0.0.8").is_some());
        assert!(find(line("host.example"), &KEY, "10.0.0.7").is_none());
        assert!(find(line("2001:db8:*"), &KEY, "2001:db8::1").is_some());
        assert!(find(line("2001:DB8::1"), &KEY, "2001:db8::1").is_some());
        assert!(find(line("*"), &KEY, "2001:db8::1").is_some());
    }

    /// Each of these would admit everyone, negating something it cannot match.
    #[test]
    fn a_from_entry_that_cannot_match_refuses_the_whole_line() {
        for pattern in [
            "!127.0.0.0/8,*",
            "!localhost,*",
            "!cafe.de,*",
            "!::ffff:127.0.0.1,*",
            "!2001:0db8::1,*",
            "!2001:db8:*,*",
            "!010.0.0.1,*",
            "!010.*,*",
            "!300.*,*",
            "!300?.*,*",
            "!10.0.0.*,*",
            "!10.*,*",
            "!*,*",
            "!?,*",
            "!1.2.3.4.5*,*",
            "!,*",
            "*,",
            "",
            "!!10.0.0.1,*",
            "fe80::1%eth0",
            "10.0.0.0/8",
            "*.example",
        ] {
            let text = format!("from=\"{pattern}\" {}", key_line(&KEY));
            assert_eq!(find(&text, &KEY, "127.0.0.1"), None, "{pattern}");
            assert_eq!(find(&text, &KEY, "10.9.9.9"), None, "{pattern}");
        }
    }

    #[test]
    fn a_repeated_option_refuses_the_whole_line() {
        for options in [
            "from=\"10.0.0.1\",from=\"*\"",
            "from=\"*\",from=\"10.0.0.1\"",
            "command=\"a\",command=\"b\"",
            "no-pty,no-pty",
            "restrict,restrict",
        ] {
            let text = format!("{options} {}", key_line(&KEY));
            assert_eq!(find(&text, &KEY, "10.9.9.9"), None, "{options}");
        }
    }

    #[test]
    fn a_line_that_is_not_utf8_costs_that_line_alone() {
        let mut text = b"# \xff not text\nssh-ed25519 \xff\xfe c\n".to_vec();
        text.extend_from_slice(key_line(&KEY).as_bytes());
        assert_eq!(find(&text, &KEY, "x"), Some(Grant::default()));
    }

    #[test]
    fn a_key_of_small_order_makes_the_line_unusable_and_costs_that_line_alone() {
        let mut identity = [0u8; 32];
        identity[0] = 1;
        let plain = key_line(&identity);
        assert_eq!(find(&plain, &identity, "x"), None);
        let with_option = format!("no-pty {plain}");
        assert_eq!(find(&with_option, &identity, "x"), None);
        let text = format!("{plain}{}", key_line(&KEY));
        assert_eq!(find(&text, &identity, "x"), None);
        assert_eq!(find(&text, &KEY, "x"), Some(Grant::default()));
    }

    #[test]
    fn a_refused_from_keeps_searching_for_a_later_line() {
        let text = format!(
            "from=\"10.0.0.1\" {}no-pty {}",
            key_line(&KEY),
            key_line(&KEY)
        );
        assert!(find(&text, &KEY, "10.0.0.2").unwrap().no_pty);
        assert!(!find(&text, &KEY, "10.0.0.1").unwrap().no_pty);
    }

    #[test]
    fn harmless_disablers_are_accepted() {
        let text = format!(
            "no-port-forwarding,no-agent-forwarding,no-X11-forwarding,no-user-rc {}",
            key_line(&KEY)
        );
        assert_eq!(find(&text, &KEY, "x"), Some(Grant::default()));
    }

    fn current_uid() -> u32 {
        fs::metadata("/proc/self").unwrap().uid()
    }

    /// A private tree the current uid owns, with no writable-by-others ancestors
    /// to depend on: the walk stops being testable under a sticky temp directory,
    /// so these exercise the file and nearest-directory rules directly.
    #[test]
    fn strict_modes_refuse_writable_files_and_wrong_types() {
        let dir = td_fs::TempDir::new("td-ssh-authkeys").unwrap();
        let file = dir.path().join("keys");
        fs::write(&file, key_line(&KEY)).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o666)).unwrap();
        let error = check_path(&file, current_uid()).unwrap_err();
        assert!(error.contains("bad ownership or modes for file"), "{error}");
        assert!(check_path(dir.path(), current_uid()).is_err());
    }

    #[test]
    fn a_writable_directory_is_refused_unless_sticky_with_a_non_root_owner() {
        let dir = td_fs::TempDir::new("td-ssh-authkeys").unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let file = sub.join("keys");
        fs::write(&file, key_line(&KEY)).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o777)).unwrap();
        let error = check_path(&file, current_uid()).unwrap_err();
        assert!(error.contains("for directory"), "{error}");
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o1777)).unwrap();
        if current_uid() != 0 {
            check_path(&file, current_uid()).unwrap();
            // Root alone may own every step of the absolute form.
            assert!(check_path(&file, 0).is_err());
        }
    }

    /// A root-only chain has no sticky exemption: this is what the real daemon's
    /// host key and every absolute key file are judged by.
    #[test]
    fn only_a_non_root_chain_trusts_a_sticky_directory() {
        const USER: u32 = 1000;
        for (uid, mode, owner, passes) in [
            (0, 0o755, 0, true),
            (0, 0o1777, 0, false),
            (0, 0o777, 0, false),
            (USER, 0o755, 0, false),
            (0, 0o755, USER, true),
            (0, 0o1777, USER, true),
            (USER, 0o1777, USER, true),
            (0, 0o777, USER, false),
            (USER, 0o775, USER, false),
            (USER + 1, 0o755, USER, false),
            (USER + 1, 0o1777, USER, false),
        ] {
            assert_eq!(
                directory_passes(uid, mode, owner),
                passes,
                "uid {uid} mode {mode:o} owner {owner}"
            );
        }
    }

    #[test]
    fn a_wrong_owner_is_refused_and_a_symlink_is_judged_by_its_target() {
        let dir = td_fs::TempDir::new("td-ssh-authkeys").unwrap();
        let file = dir.path().join("keys");
        fs::write(&file, key_line(&KEY)).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        // Root owning the fixture is always acceptable, so this needs a non-root run.
        if current_uid() != 0 {
            assert!(check_path(&file, current_uid().wrapping_add(1)).is_err());
            assert!(check_path(&file, 0).is_err());
        }
        let link = dir.path().join("link");
        symlink(&file, &link).unwrap();
        assert!(check_path(&link, current_uid()).is_err());
        // read_checked resolves it first, then judges the target's own path.
        assert_eq!(
            read_checked(&dir.path().join("missing"), current_uid()),
            Err(KeyFileError::Missing)
        );
    }
}
