//! A server's provisioned primary account (td-svc/DESIGN.md §10,
//! "Profiles"): the name the primary account takes and the Ed25519 public
//! keys its root-owned `/var/lib/td/authorized-keys` admits, staged into a
//! persistent volume's seed where `td-install` stages its settings.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use td_recipe::types::Step;

// Both are boot-harness sources, outside every recipe check's key
// (recipes/build.rs `HOST_CHECK_SOURCES`): strict base64 for key blobs, and
// the guest's own primary-name grammar.
#[path = "../../../../../td-encoding/src/lib.rs"]
#[allow(dead_code, reason = "only the base64 pair serves key blobs")]
pub(crate) mod encoding;
#[path = "../../../../../td-authd/src/primary_account.rs"]
#[allow(
    dead_code,
    reason = "only the name grammar and passwd parse serve the host rule"
)]
mod primary_account;
use encoding::from_base64;

/// Seed-relative; the guest's `prepare-primary-profile` reads the first and
/// its sshd policy names the second.
const USERNAME: &str = "@var/lib/td/username";
const AUTHORIZED_KEYS: &str = "@var/lib/td/authorized-keys";
const KEY_TYPE: &str = "ssh-ed25519";
const MAX_KEYS: usize = 64;
const MAX_COMMENT: usize = 256;
/// The account forms td-firstboot reserves for sessions and applications:
/// one of these followed by decimal digits.
const RESERVED_ACCOUNT_PREFIXES: &[&str] = &["tda", "tdb", "tdc", "tdi", "tdp"];

pub(crate) struct ProvisionedAccount {
    name: String,
    keys: Vec<String>,
}

impl ProvisionedAccount {
    /// The primary account of the system recipe `stem`, renamed `name`.
    /// `public_keys` holds OpenSSH public key lines; blank and `#` lines are
    /// skipped and a repeated key is kept once. At least one key is required:
    /// a server's only login is SSH.
    pub(crate) fn new(stem: &str, name: &str, public_keys: &str) -> Result<Self, String> {
        check_name(stem, name).map_err(|error| format!("account name {name:?}: {error}"))?;
        let mut keys: Vec<String> = Vec::new();
        for (number, line) in public_keys.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let key = parse_key(line)
                .map_err(|error| format!("public key line {}: {error}", number + 1))?;
            let blob = key.split(' ').nth(1);
            if !keys.iter().any(|kept| kept.split(' ').nth(1) == blob) {
                keys.push(key);
            }
        }
        if keys.is_empty() {
            return Err("a server account needs at least one ssh-ed25519 public key".into());
        }
        if keys.len() > MAX_KEYS {
            return Err(format!(
                "{} public keys exceed the {MAX_KEYS}-key bound",
                keys.len()
            ));
        }
        Ok(Self {
            name: name.to_owned(),
            keys,
        })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Writes both files under `seed`, mode 0644 in directories of mode
    /// 0755: `mkfs.btrfs --rootdir` copies modes verbatim, and the guest
    /// requires exactly these.
    pub(crate) fn stage(&self, seed: &Path) -> Result<(), String> {
        for directory in ["@var/lib", "@var/lib/td"] {
            let path = seed.join(directory);
            fs::create_dir_all(&path)
                .and_then(|()| fs::set_permissions(&path, fs::Permissions::from_mode(0o755)))
                .map_err(|error| format!("create {}: {error}", path.display()))?;
        }
        let mut keys = self.keys.join("\n");
        keys.push('\n');
        for (relative, contents) in [
            (USERNAME, format!("{}\n", self.name)),
            (AUTHORIZED_KEYS, keys),
        ] {
            let path = seed.join(relative);
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .open(&path)
                .and_then(|mut file| {
                    file.write_all(contents.as_bytes())?;
                    file.set_permissions(fs::Permissions::from_mode(0o644))
                })
                .map_err(|error| format!("write {}: {error}", path.display()))?;
        }
        Ok(())
    }
}

/// The rule td-firstboot's `check-primary-name` holds a saved name to: the
/// guest's grammar, no reserved session or application form, and no account
/// or group but the primary's own in the image's tables, read from the
/// recipe's own `/etc` writes. The guest refuses any other name at boot,
/// which would leave the image unreachable, so the host refuses it first.
fn check_name(stem: &str, name: &str) -> Result<(), String> {
    primary_account::validate_name(name).map_err(|error| error.to_string())?;
    if RESERVED_ACCOUNT_PREFIXES.iter().any(|prefix| {
        name.strip_prefix(prefix).is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
    }) {
        return Err("the name is reserved for a session or application account".into());
    }
    let passwd = etc_table(stem, "passwd")?;
    let primary = primary_account::parse(&passwd).map_err(|error| error.to_string())?;
    let taken = [passwd, etc_table(stem, "group")?].iter().any(|table| {
        table
            .lines()
            .filter_map(|line| line.split(':').next())
            .any(|account| account == name && account != primary.name())
    });
    if taken {
        return Err(format!(
            "{stem} already has an account or group of that name"
        ));
    }
    Ok(())
}

/// The one `/etc/NAME` the system recipe writes into its root.
fn etc_table(stem: &str, name: &str) -> Result<String, String> {
    let recipe = td_recipe::catalog::lookup(stem).ok_or_else(|| format!("no recipe {stem}"))?;
    let path = format!("{{root}}/real-root/etc/{name}");
    let mut written = recipe.steps.iter().flatten().filter_map(|step| match step {
        Step::WriteFile {
            path: at, content, ..
        } if *at == path => Some(content.clone()),
        _ => None,
    });
    match (written.next(), written.next()) {
        (Some(table), None) => Ok(table),
        _ => Err(format!("{stem} does not write exactly one {path}")),
    }
}

/// One `ssh-ed25519 BASE64 [comment]` line, re-spelled with single spaces.
/// The blob must be exactly the RFC 8709 public key encoding; options,
/// other key types and control characters are refused.
fn parse_key(line: &str) -> Result<String, String> {
    let mut fields = line.split_ascii_whitespace();
    let kind = fields.next().unwrap_or_default();
    if kind != KEY_TYPE {
        return Err(format!(
            "only {KEY_TYPE} keys are admitted (td's OpenSSH is built without \
             libcrypto), and a line may carry no options; found {:?}",
            kind.chars().take(32).collect::<String>()
        ));
    }
    let blob = fields.next().ok_or("the key has no base64 blob")?;
    let bytes = from_base64(blob).ok_or("the key blob is not canonical base64")?;
    let mut expected = Vec::with_capacity(51);
    expected.extend_from_slice(&11u32.to_be_bytes());
    expected.extend_from_slice(KEY_TYPE.as_bytes());
    expected.extend_from_slice(&32u32.to_be_bytes());
    if bytes.len() != 51 || bytes.get(..19) != Some(expected.as_slice()) {
        return Err(format!(
            "the key blob is not one {KEY_TYPE} public key (51 bytes)"
        ));
    }
    let comment = fields.collect::<Vec<_>>().join(" ");
    if comment.len() > MAX_COMMENT || !comment.bytes().all(|byte| (0x20..0x7f).contains(&byte)) {
        return Err(format!(
            "the key comment must be printable ASCII of at most {MAX_COMMENT} bytes"
        ));
    }
    Ok(if comment.is_empty() {
        format!("{KEY_TYPE} {blob}")
    } else {
        format!("{KEY_TYPE} {blob} {comment}")
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::encoding::base64;
    use super::*;
    use std::os::unix::fs::MetadataExt;

    const STEM: &str = "server-x86-64";

    /// The host's rule refuses each name the guest's registry would: every
    /// other account and group in the image, and the reserved forms.
    #[test]
    fn the_name_rule_is_the_guests() {
        let passwd = etc_table(STEM, "passwd").unwrap();
        let group = etc_table(STEM, "group").unwrap();
        let primary = primary_account::parse(&passwd).unwrap();
        assert_eq!(check_name(STEM, primary.name()), Ok(()));
        for name in ["ada", "operator", "test", "tda", "tdb-1", "tdax1"] {
            assert_eq!(check_name(STEM, name), Ok(()), "{name}");
        }
        let mut others = 0;
        for table in [&passwd, &group] {
            for account in table.lines().filter_map(|line| line.split(':').next()) {
                if account != primary.name() {
                    assert!(check_name(STEM, account).is_err(), "{account}");
                    others += 1;
                }
            }
        }
        assert!(others > 4, "{others}");
        for name in [
            "root", "sshd", "wheel", "tda1000", "tdb1", "tdc2", "tdi3", "tdp4", "", "Ada", "1ada",
        ] {
            assert!(check_name(STEM, name).is_err(), "{name:?}");
        }
        assert!(
            include_str!("../../../../../td-firstboot/src/principals.rs")
                .contains(r#"if ["tda", "tdb", "tdc", "tdi", "tdp"].iter().any(|prefix| {"#)
        );
        assert_eq!(
            RESERVED_ACCOUNT_PREFIXES,
            ["tda", "tdb", "tdc", "tdi", "tdp"]
        );
        assert!(check_name("td-no-such-recipe", "ada").is_err());
    }

    fn blob(kind: &str, key: &[u8]) -> String {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&u32::try_from(kind.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(kind.as_bytes());
        bytes.extend_from_slice(&u32::try_from(key.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(key);
        base64(&bytes)
    }

    #[test]
    fn ed25519_lines_are_admitted_respelled_and_deduplicated() {
        let one = blob(KEY_TYPE, &[7; 32]);
        let two = blob(KEY_TYPE, &[9; 32]);
        let account = ProvisionedAccount::new(
            STEM,
            "ada",
            &format!(
                "# keys\n\nssh-ed25519   {one}  ada@host  laptop\n{KEY_TYPE} {two}\n{KEY_TYPE} {one} again\n"
            ),
        )
        .unwrap();
        assert_eq!(account.name(), "ada");
        assert_eq!(
            account.keys,
            [
                format!("{KEY_TYPE} {one} ada@host laptop"),
                format!("{KEY_TYPE} {two}")
            ]
        );
    }

    #[test]
    fn anything_but_one_ed25519_key_a_line_is_refused() {
        let good = blob(KEY_TYPE, &[7; 32]);
        for line in [
            format!("ssh-rsa {good}"),
            format!("from=\"10.0.0.1\" {KEY_TYPE} {good}"),
            KEY_TYPE.to_string(),
            format!("{KEY_TYPE} not-base64!"),
            format!("{KEY_TYPE} {}", &good[1..]),
            format!("{KEY_TYPE} {}", blob(KEY_TYPE, &[7; 31])),
            format!("{KEY_TYPE} {}", blob(KEY_TYPE, &[7; 33])),
            format!("{KEY_TYPE} {}", blob("ssh-ed25518", &[7; 32])),
            format!("{KEY_TYPE} {good} tab\u{7f}"),
            format!("{KEY_TYPE} {good} caf\u{e9}"),
            format!("{KEY_TYPE} {good} {}", "x".repeat(MAX_COMMENT + 1)),
        ] {
            assert!(parse_key(&line).is_err(), "{line}");
            assert!(
                ProvisionedAccount::new(STEM, "ada", &line).is_err(),
                "{line}"
            );
        }
        assert!(parse_key(&format!("{KEY_TYPE} {good} {}", "x".repeat(MAX_COMMENT))).is_ok());
    }

    #[test]
    fn the_account_needs_a_valid_name_and_a_bounded_nonempty_key_set() {
        let line = format!("{KEY_TYPE} {}\n", blob(KEY_TYPE, &[7; 32]));
        for name in ["", "root", "Ada", "1ada", "ada smith"] {
            assert!(
                ProvisionedAccount::new(STEM, name, &line).is_err(),
                "{name:?}"
            );
        }
        assert!(ProvisionedAccount::new(STEM, "ada", "").is_err());
        assert!(ProvisionedAccount::new(STEM, "ada", "# only a comment\n").is_err());
        let many: String = (0..=MAX_KEYS)
            .map(|index| {
                let mut key = [0u8; 32];
                key[..8].copy_from_slice(&(index as u64).to_be_bytes());
                format!("{KEY_TYPE} {}\n", blob(KEY_TYPE, &key))
            })
            .collect();
        assert!(ProvisionedAccount::new(STEM, "ada", &many).is_err());
        let bounded: String = many
            .lines()
            .skip(1)
            .map(|line| format!("{line}\n"))
            .collect();
        assert_eq!(
            ProvisionedAccount::new(STEM, "ada", &bounded)
                .unwrap()
                .keys
                .len(),
            MAX_KEYS
        );
    }

    #[test]
    fn staging_writes_both_files_with_the_modes_the_guest_requires() {
        let seed = crate::test_temp::TempDir::new("td-provisioned-account").unwrap();
        fs::create_dir(seed.path().join("@var")).unwrap();
        let key = blob(KEY_TYPE, &[7; 32]);
        let account =
            ProvisionedAccount::new(STEM, "ada", &format!("{KEY_TYPE} {key} c\n")).unwrap();
        account.stage(seed.path()).unwrap();
        let mode = |relative: &str| {
            fs::symlink_metadata(seed.path().join(relative))
                .unwrap()
                .mode()
                & 0o7777
        };
        assert_eq!(
            fs::read_to_string(seed.path().join(USERNAME)).unwrap(),
            "ada\n"
        );
        assert_eq!(
            fs::read_to_string(seed.path().join(AUTHORIZED_KEYS)).unwrap(),
            format!("{KEY_TYPE} {key} c\n")
        );
        for (relative, expected) in [
            ("@var/lib", 0o755),
            ("@var/lib/td", 0o755),
            (USERNAME, 0o644),
            (AUTHORIZED_KEYS, 0o644),
        ] {
            assert_eq!(mode(relative), expected, "{relative}");
        }
        // A second stage would overwrite what an earlier writer left: refused.
        assert!(account.stage(seed.path()).is_err());
    }

    /// The guest names the same paths: its sshd policy and the saved name.
    #[test]
    fn the_seed_paths_are_the_guests() {
        let policy = include_str!("../../../../../td-firstboot/src/ssh_policy.rs");
        assert!(policy.contains(&format!(
            "pub const SSHD_PROVISIONED_AUTHORIZED_KEYS: &str = \"/var{}\";",
            AUTHORIZED_KEYS.strip_prefix("@var").unwrap()
        )));
        let profile = include_str!("../../../../../td-firstboot/src/primary_profile.rs");
        assert!(profile.contains(&format!(
            "self.root.join(\"var{}\")",
            USERNAME.strip_prefix("@var").unwrap()
        )));
    }
}
