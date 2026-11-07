//! `td-sshd -f FILE`: a strict subset of `sshd_config`. Anything unrecognized
//! refuses the whole file, since a policy half-understood is the wrong failure
//! for the program that decides who logs in (DESIGN.md section 5).

use crate::kex::{CIPHER, DEFAULT_KEX, HOST_KEY_ALGORITHM};
use std::net::IpAddr;
use std::path::PathBuf;

const DEFAULT_LOGIN_GRACE: u64 = 30;
const DEFAULT_AUTH_TRIES: u32 = 3;
const DEFAULT_SESSIONS: u32 = 4;
const MAX_LOGIN_GRACE: u64 = 600;
const MAX_AUTH_TRIES: u32 = 6;
const MAX_SESSIONS: u32 = 16;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MatchBlock {
    pub user: String,
    pub authorized_keys: Option<Vec<String>>,
    pub force_command: Option<String>,
    pub permit_tty: Option<bool>,
    pub max_sessions: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    pub port: u16,
    pub listen: IpAddr,
    pub host_key: PathBuf,
    pub authorized_keys: Vec<String>,
    pub allow_users: Option<Vec<String>>,
    pub login_grace_seconds: u64,
    pub max_auth_tries: u32,
    pub max_sessions: u32,
    pub matches: Vec<MatchBlock>,
}

/// One account's view of the configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Effective {
    pub authorized_keys: Vec<String>,
    pub force_command: Option<String>,
    pub permit_tty: bool,
    pub max_sessions: u32,
}

/// The shared account validation's shape: no pattern characters reach matching.
pub fn valid_account_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
}

fn parse_bool(keyword: &str, value: &str) -> Result<bool, String> {
    match value {
        "yes" => Ok(true),
        "no" => Ok(false),
        _ => Err(format!("{keyword}: expected yes or no, not {value}")),
    }
}

fn bounded(keyword: &str, value: &str, low: u64, high: u64) -> Result<u64, String> {
    let number: u64 = value
        .parse()
        .map_err(|_| format!("{keyword}: bad number {value}"))?;
    if !(low..=high).contains(&number) {
        return Err(format!("{keyword}: {number} is outside {low}..={high}"));
    }
    Ok(number)
}

fn key_files(value: &str) -> Result<Vec<String>, String> {
    let files: Vec<String> = value.split_ascii_whitespace().map(String::from).collect();
    if files.is_empty() {
        return Err("AuthorizedKeysFile: no file named".to_string());
    }
    // OpenSSH gives these meanings this server does not implement, and reading
    // them as literal file names would quietly grant or refuse the wrong thing.
    if let Some(file) = files
        .iter()
        .find(|file| file.eq_ignore_ascii_case("none") || file.contains('%'))
    {
        return Err(format!(
            "AuthorizedKeysFile {file}: neither none nor a % token is supported"
        ));
    }
    Ok(files)
}

fn force_command(value: &str) -> Result<String, String> {
    if value.eq_ignore_ascii_case("none") {
        return Err("ForceCommand none: a forced command cannot be unset".to_string());
    }
    Ok(value.to_string())
}

/// A single-valued keyword given twice would be read last-wins.
fn once<T>(slot: &Option<T>, keyword: &str) -> Result<(), String> {
    match slot {
        Some(_) => Err(format!("{keyword}: given more than once")),
        None => Ok(()),
    }
}

impl ServerConfig {
    pub fn parse(text: &str, allow_port_zero: bool) -> Result<Self, String> {
        let mut port = None;
        let mut listen = None;
        let mut host_key = None;
        let mut authorized_keys = None;
        let mut allow_users: Option<Vec<String>> = None;
        let mut permit_root_login = None;
        let mut login_grace_seconds = None;
        let mut max_auth_tries = None;
        let mut max_sessions = None;
        let mut matches: Vec<MatchBlock> = Vec::new();

        for (index, raw) in text.lines().enumerate() {
            let located = |error: String| format!("line {}: {error}", index + 1);
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (keyword, value) = line
                .split_once([' ', '\t'])
                .ok_or_else(|| located(format!("{line}: missing value")))?;
            let value = value.trim();
            let keyword = keyword.to_ascii_lowercase();
            if keyword == "match" {
                let mut words = value.split_ascii_whitespace();
                let (Some("User"), Some(user), None) = (words.next(), words.next(), words.next())
                else {
                    return Err(located(format!("Match {value}: only Match User NAME")));
                };
                if !valid_account_name(user) {
                    return Err(located(format!("Match User {user}: bad account name")));
                }
                matches.push(MatchBlock {
                    user: user.to_string(),
                    ..MatchBlock::default()
                });
                continue;
            }
            if let Some(block) = matches.last_mut() {
                match keyword.as_str() {
                    "authorizedkeysfile" => {
                        once(&block.authorized_keys, "AuthorizedKeysFile").map_err(located)?;
                        block.authorized_keys = Some(key_files(value).map_err(located)?);
                    }
                    "forcecommand" => {
                        once(&block.force_command, "ForceCommand").map_err(located)?;
                        block.force_command = Some(force_command(value).map_err(located)?);
                    }
                    "permittty" => {
                        once(&block.permit_tty, "PermitTTY").map_err(located)?;
                        block.permit_tty = Some(parse_bool("PermitTTY", value).map_err(located)?);
                    }
                    "maxsessions" => {
                        once(&block.max_sessions, "MaxSessions").map_err(located)?;
                        let sessions = bounded("MaxSessions", value, 1, u64::from(MAX_SESSIONS))
                            .map_err(located)?;
                        block.max_sessions = u32::try_from(sessions).ok();
                    }
                    other => return Err(located(format!("{other}: not allowed in a Match block"))),
                }
                continue;
            }
            match keyword.as_str() {
                "port" => {
                    once(&port, "Port").map_err(located)?;
                    let low = u64::from(!allow_port_zero);
                    let number = bounded("Port", value, low, 65535).map_err(located)?;
                    port = u16::try_from(number).ok();
                }
                "listenaddress" => {
                    once(&listen, "ListenAddress").map_err(located)?;
                    listen = Some(
                        value
                            .parse::<IpAddr>()
                            .map_err(|_| located(format!("ListenAddress: bad address {value}")))?,
                    );
                }
                "hostkey" => {
                    once(&host_key, "HostKey").map_err(located)?;
                    host_key = Some(PathBuf::from(value));
                }
                "authorizedkeysfile" => {
                    once(&authorized_keys, "AuthorizedKeysFile").map_err(located)?;
                    authorized_keys = Some(key_files(value).map_err(located)?);
                }
                "allowusers" => {
                    once(&allow_users, "AllowUsers").map_err(located)?;
                    let mut names = Vec::new();
                    for name in value.split_ascii_whitespace() {
                        if !valid_account_name(name) {
                            return Err(located(format!("AllowUsers: bad account name {name}")));
                        }
                        names.push(name.to_string());
                    }
                    allow_users = Some(names);
                }
                "permitrootlogin" => {
                    once(&permit_root_login, "PermitRootLogin").map_err(located)?;
                    permit_root_login = Some(());
                    if value != "no" {
                        return Err(located(format!(
                            "PermitRootLogin {value}: td-sshd never admits root"
                        )));
                    }
                }
                "logingracetime" => {
                    once(&login_grace_seconds, "LoginGraceTime").map_err(located)?;
                    login_grace_seconds = Some(
                        bounded("LoginGraceTime", value, 1, MAX_LOGIN_GRACE).map_err(located)?,
                    );
                }
                "maxauthtries" => {
                    once(&max_auth_tries, "MaxAuthTries").map_err(located)?;
                    let tries = bounded("MaxAuthTries", value, 1, u64::from(MAX_AUTH_TRIES))
                        .map_err(located)?;
                    max_auth_tries = Some(u32::try_from(tries).unwrap_or(MAX_AUTH_TRIES));
                }
                "maxsessions" => {
                    once(&max_sessions, "MaxSessions").map_err(located)?;
                    let sessions = bounded("MaxSessions", value, 1, u64::from(MAX_SESSIONS))
                        .map_err(located)?;
                    max_sessions = Some(u32::try_from(sessions).unwrap_or(MAX_SESSIONS));
                }
                other => return Err(located(format!("{other}: unknown keyword"))),
            }
        }
        Ok(Self {
            port: port.ok_or("Port is required")?,
            listen: listen.ok_or("ListenAddress is required")?,
            host_key: host_key.ok_or("HostKey is required")?,
            authorized_keys: authorized_keys.ok_or("AuthorizedKeysFile is required")?,
            allow_users,
            login_grace_seconds: login_grace_seconds.unwrap_or(DEFAULT_LOGIN_GRACE),
            max_auth_tries: max_auth_tries.unwrap_or(DEFAULT_AUTH_TRIES),
            max_sessions: max_sessions.unwrap_or(DEFAULT_SESSIONS),
            matches,
        })
    }

    /// For each keyword, the first `Match User` block setting it wins, as in
    /// OpenSSH, over the global value. Walking backwards lets earlier blocks
    /// overwrite later ones.
    pub fn effective(&self, user: &str) -> Effective {
        let mut effective = Effective {
            authorized_keys: self.authorized_keys.clone(),
            force_command: None,
            permit_tty: true,
            max_sessions: self.max_sessions,
        };
        for block in self.matches.iter().rev().filter(|block| block.user == user) {
            if let Some(files) = &block.authorized_keys {
                effective.authorized_keys = files.clone();
            }
            if block.force_command.is_some() {
                effective.force_command = block.force_command.clone();
            }
            if let Some(permit) = block.permit_tty {
                effective.permit_tty = permit;
            }
            if let Some(sessions) = block.max_sessions {
                effective.max_sessions = sessions;
            }
        }
        effective
    }

    pub fn admits_name(&self, user: &str) -> bool {
        self.allow_users
            .as_ref()
            .is_none_or(|names| names.iter().any(|name| name == user))
    }

    /// `-T`: the policy as it applies, lowercased as `sshd -T` prints it, with
    /// the compiled behaviour stated so a reader sees all of it.
    pub fn describe(&self, user: Option<&str>) -> String {
        let effective = match user {
            Some(user) => self.effective(user),
            None => Effective {
                authorized_keys: self.authorized_keys.clone(),
                force_command: None,
                permit_tty: true,
                max_sessions: self.max_sessions,
            },
        };
        let mut out = format!(
            "port {}\nlistenaddress {}\nhostkey {}\npermitrootlogin no\n",
            self.port,
            self.listen,
            self.host_key.display()
        );
        match &self.allow_users {
            Some(names) => {
                for name in names {
                    out.push_str(&format!("allowusers {name}\n"));
                }
            }
            None => out.push_str("allowusers none\n"),
        }
        out.push_str(&format!(
            "authorizedkeysfile {}\n",
            effective.authorized_keys.join(" ")
        ));
        out.push_str(&format!(
            "kexalgorithms {}\nhostkeyalgorithms {HOST_KEY_ALGORITHM}\npubkeyacceptedalgorithms {HOST_KEY_ALGORITHM}\nciphers {CIPHER}\n",
            DEFAULT_KEX.join(",")
        ));
        out.push_str(
            "authenticationmethods publickey\npasswordauthentication no\nstrictmodes yes\ndisableforwarding yes\npermituserenvironment no\npermituserrc no\nusedns no\ncompression no\n",
        );
        out.push_str(&format!(
            "logingracetime {}\nmaxauthtries {}\nmaxsessions {}\npermittty {}\n",
            self.login_grace_seconds,
            self.max_auth_tries,
            effective.max_sessions,
            if effective.permit_tty { "yes" } else { "no" }
        ));
        if let Some(command) = &effective.force_command {
            out.push_str(&format!("forcecommand {command}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const BASE: &str = "Port 22\nListenAddress 0.0.0.0\nHostKey /k\nAuthorizedKeysFile /a\n";

    /// Last-wins would have a line near the end quietly undo one near the top.
    #[test]
    fn every_keyword_refuses_a_repeat_in_its_scope() {
        let required = "Port 22\nListenAddress 127.0.0.1\nHostKey /k\nAuthorizedKeysFile a\n";
        for repeated in [
            "Port 23",
            "ListenAddress ::1",
            "HostKey /other",
            "AuthorizedKeysFile b",
        ] {
            let text = format!("{required}{repeated}\n");
            let error = ServerConfig::parse(&text, false).unwrap_err();
            assert!(error.contains("more than once"), "{repeated}: {error}");
        }
        for twice in [
            "AllowUsers a\nAllowUsers b\n",
            "PermitRootLogin no\nPermitRootLogin no\n",
            "LoginGraceTime 3\nLoginGraceTime 4\n",
            "MaxAuthTries 1\nMaxAuthTries 2\n",
            "MaxSessions 1\nMaxSessions 2\n",
        ] {
            let error = ServerConfig::parse(&format!("{required}{twice}"), false).unwrap_err();
            assert!(error.contains("more than once"), "{twice}: {error}");
        }
        for twice in [
            "AuthorizedKeysFile a\n\tAuthorizedKeysFile b\n",
            "ForceCommand a\nForceCommand b\n",
            "PermitTTY no\nPermitTTY yes\n",
            "MaxSessions 1\nMaxSessions 2\n",
        ] {
            let text = format!("{required}Match User a\n{twice}");
            let error = ServerConfig::parse(&text, false).unwrap_err();
            assert!(error.contains("more than once"), "{twice}: {error}");
        }
    }

    /// A scope is the global part or one block: each may say each thing once.
    #[test]
    fn a_keyword_may_appear_once_in_each_scope() {
        let text = "Port 22\nListenAddress 127.0.0.1\nHostKey /k\nAuthorizedKeysFile a\nMaxSessions 2\nMatch User a\nAuthorizedKeysFile b\nMaxSessions 3\nMatch User b\nAuthorizedKeysFile c\nMaxSessions 4\n";
        let config = ServerConfig::parse(text, false).unwrap();
        assert_eq!(config.max_sessions, 2);
        assert_eq!(config.effective("a").max_sessions, 3);
        assert_eq!(config.effective("b").max_sessions, 4);
        assert_eq!(config.effective("c").authorized_keys, ["a"]);
    }

    #[test]
    fn a_full_policy_parses() {
        let text = format!(
            "{BASE}PermitRootLogin no\nAllowUsers alice td-admin\nLoginGraceTime 30\nMaxAuthTries 3\nMaxSessions 4\nMatch User td-admin\n\tAuthorizedKeysFile /var/lib/td-admin/authorized_keys\n\tForceCommand /bin/td-admin ssh\n\tPermitTTY no\n\tMaxSessions 1\nMatch User alice\n\tAuthorizedKeysFile /run/a /run/b .ssh/authorized_keys\n"
        );
        let config = ServerConfig::parse(&text, false).unwrap();
        assert_eq!(config.port, 22);
        assert!(config.admits_name("alice"));
        assert!(!config.admits_name("mallory"));
        let admin = config.effective("td-admin");
        assert_eq!(admin.force_command.as_deref(), Some("/bin/td-admin ssh"));
        assert!(!admin.permit_tty);
        assert_eq!(admin.max_sessions, 1);
        let alice = config.effective("alice");
        assert_eq!(
            alice.authorized_keys,
            ["/run/a", "/run/b", ".ssh/authorized_keys"]
        );
        assert_eq!(alice.force_command, None);
        assert!(alice.permit_tty);
        assert_eq!(alice.max_sessions, 4);
        assert_eq!(config.effective("other").authorized_keys, ["/a"]);
    }

    #[test]
    fn the_first_match_block_setting_a_keyword_wins() {
        let text = format!(
            "{BASE}Match User a\n\tForceCommand first\nMatch User a\n\tForceCommand second\n\tPermitTTY no\n"
        );
        let effective = ServerConfig::parse(&text, false).unwrap().effective("a");
        assert_eq!(effective.force_command.as_deref(), Some("first"));
        assert!(!effective.permit_tty);
    }

    #[test]
    fn without_allow_users_every_name_passes_this_check() {
        let config = ServerConfig::parse(BASE, false).unwrap();
        assert!(config.admits_name("anyone"));
    }

    #[test]
    fn anything_unrecognized_refuses_the_whole_file() {
        for extra in [
            "PasswordAuthentication yes",
            "PermitRootLogin yes",
            "PermitRootLogin prohibit-password",
            "Subsystem sftp internal-sftp",
            "Subsystem sftp /usr/lib/sftp-server",
            "Match Address 10.0.0.1",
            "Match User alice Host x",
            "Match User ALICE",
            "AllowUsers al*ce",
            "MaxAuthTries 100",
            "MaxSessions 0",
            "LoginGraceTime 0",
            "NotAKeyword",
            "AuthorizedKeysFile",
            "Match User a\nPort 22",
            "Match User a\nForwardAgent yes",
            "Port 23",
            "ListenAddress 127.0.0.1",
            "HostKey /other",
            "AuthorizedKeysFile none",
            "AuthorizedKeysFile /a none",
            "AuthorizedKeysFile %h/.ssh/authorized_keys",
            "AuthorizedKeysFile /a /b/%u",
            "Match User a\nAuthorizedKeysFile none",
            "Match User a\nAuthorizedKeysFile .ssh/%u",
            "Match User a\nForceCommand none",
            "Match User a\nForceCommand NONE",
        ] {
            let text = format!("{BASE}{extra}\n");
            assert!(ServerConfig::parse(&text, false).is_err(), "{extra}");
        }
    }

    #[test]
    fn required_keywords_are_required_and_port_zero_is_gated() {
        assert!(ServerConfig::parse("Port 22\n", false).is_err());
        let zero = "Port 0\nListenAddress 127.0.0.1\nHostKey /k\nAuthorizedKeysFile /a\n";
        assert!(ServerConfig::parse(zero, false).is_err());
        assert!(ServerConfig::parse(zero, true).is_ok());
    }

    #[test]
    fn describe_states_the_compiled_policy() {
        let text = format!("{BASE}AllowUsers alice\nMatch User a\n\tForceCommand x\n");
        let config = ServerConfig::parse(&text, false).unwrap();
        let described = config.describe(Some("a"));
        for line in [
            "permitrootlogin no",
            "allowusers alice",
            "passwordauthentication no",
            "disableforwarding yes",
            "ciphers chacha20-poly1305@openssh.com",
            "forcecommand x",
        ] {
            assert!(described.lines().any(|seen| seen == line), "{line}");
        }
        assert!(!described.contains("subsystem"));
    }

    #[test]
    fn account_names_follow_the_shared_validation() {
        for good in ["a", "tester", "td-admin", "a_b1"] {
            assert!(valid_account_name(good), "{good}");
        }
        for bad in ["", "1a", "A", "a b", "a*", "-a", "a,b", "a@b"] {
            assert!(!valid_account_name(bad), "{bad}");
        }
    }
}
