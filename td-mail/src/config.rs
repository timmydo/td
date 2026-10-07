use crate::regex::UserRegex;
use std::fs;
use std::path::Path;
use td_toml::{Error as TomlError, Toml};

#[derive(Debug, Clone)]
pub struct AccountConfig {
    pub name: String,
    pub well_known_url: String,
    pub username: String,
    pub password: PasswordSource,
}

/// Where an account's password comes from. `Command` runs a shell command
/// and takes its stdout; `Portal` asks td's credential portal for the secret
/// stored under the account's name, which needs no shell and no file in the
/// jail. Exactly one is configured per account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasswordSource {
    Command(String),
    Portal(String),
}

/// The one value `secret` takes. td-secret stores the credential as
/// mail/NAME for `[account.NAME]` and `td-secret get NAME` returns it.
const PORTAL: &str = "portal";

fn password_source(
    password_command: Option<String>,
    secret: Option<String>,
    password_file: bool,
    name: &str,
    section: &str,
) -> Result<PasswordSource, ConfigError> {
    if password_file {
        return Err(ConfigError::Parse(format!(
            "password_file in {} is no longer a password source: submit the password from the human session with `td-secret set mail/{} < file`, press Ctrl+Alt+Esc then W, verify the target and touch the token, and set secret = \"{}\"",
            section, name, PORTAL
        )));
    }
    match (password_command, secret.as_deref()) {
        (Some(command), None) => Ok(PasswordSource::Command(command)),
        (None, Some(PORTAL)) => portal_name(name, section).map(PasswordSource::Portal),
        (None, Some(_)) => Err(ConfigError::Parse(format!(
            "secret in {} takes only \"{}\"",
            section, PORTAL
        ))),
        (Some(_), Some(_)) => Err(ConfigError::Parse(format!(
            "both password_command and secret set in {}; choose one",
            section
        ))),
        (None, None) => Err(ConfigError::Parse(format!(
            "missing password_command or secret in {}",
            section
        ))),
    }
}

/// The portal names a credential as td-secret does: 1 to 64 bytes of
/// `[A-Za-z0-9_-]`. The account name is that name, so an account the portal
/// cannot name is a configuration error here, not a failed lookup at connect.
fn portal_name(name: &str, section: &str) -> Result<String, ConfigError> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if valid {
        Ok(name.to_string())
    } else {
        Err(ConfigError::Parse(format!(
            "{}: the portal cannot name this account; a credential name is 1 to 64 bytes of [A-Za-z0-9_-]",
            section
        )))
    }
}

#[derive(Debug)]
pub struct Config {
    pub accounts: Vec<AccountConfig>,
    pub ui: UiConfig,
    pub mail: MailConfig,
    pub spam: SpamConfig,
}

/// Tunables for the built-in Bayesian spam classifier. The classifier scores
/// new INBOX messages and annotates synthetic `X-Tmc-Spam-Score` /
/// `X-Tmc-Spam-Verdict` headers; rules.toml decides what to do with them.
#[derive(Debug, Clone)]
pub struct SpamConfig {
    pub enabled: bool,
    /// Score at or above which a message is labelled `spam`.
    pub threshold: f64,
    /// Score at or below which a message is labelled `ham`; in between is `unsure`.
    pub ham_threshold: f64,
    /// Minimum trained messages *per class* before any verdict is emitted
    /// (cold-start gate). Below this, scoring is skipped entirely.
    pub min_training: u32,
}

#[derive(Debug)]
pub struct UiConfig {
    pub browser: Option<String>,
    pub page_size: u32,
    pub mouse: bool,
    pub sync_interval_secs: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RetentionPolicyConfig {
    pub name: String,
    pub folder: String,
    pub days: u32,
}

#[derive(Debug)]
pub struct MailConfig {
    pub archive_folder: String,
    pub deleted_folder: String,
    pub archive_mailbox_id: Option<String>,
    pub deleted_mailbox_id: Option<String>,
    pub reply_from: Option<String>,
    /// Compiled at load, so the pattern is refused where the file is read.
    pub rules_mailbox_regex: UserRegex,
    pub my_email_regex: UserRegex,
    pub retention_policies: Vec<RetentionPolicyConfig>,
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "failed to read config file: {}", e),
            ConfigError::Parse(e) => write!(f, "failed to parse config file: {}", e),
        }
    }
}

/// `[theme]`: the terminal's colours, which the window does not read
/// (it draws with the toolkit's); the section and its keys are accepted
/// so a configuration written for the terminal still loads, and checked,
/// as every raw section is, so a typo is still a typo.
const THEME_KEYS: &[&str] = &[
    "bg",
    "fg",
    "bold_fg",
    "selection_bg",
    "selection_fg",
    "status_bg",
    "status_fg",
    "header_fg",
];

#[derive(Debug)]
struct RawConfig {
    ui: RawUiConfig,
    mail: RawMailConfig,
    jmap: Option<RawAccountFields>,
    /// Named sections, in NAME order rather than document order: serde read
    /// these into a `BTreeMap`, and the account and retention lists it
    /// produced were sorted.
    account: Vec<(String, RawAccountFields)>,
    retention: Vec<(String, RawRetentionPolicy)>,
    spam: RawSpamConfig,
}

const CONFIG_KEYS: &[&str] = &[
    "ui",
    "mail",
    "jmap",
    "account",
    "retention",
    "spam",
    "theme",
];

/// The `[section.NAME]` sub-tables of `section`, sorted by name.
fn named_sections<'a>(
    root: &'a Toml,
    section: &str,
) -> Result<Vec<(&'a str, &'a Toml)>, TomlError> {
    let Some(table) = root.optional_table(section)? else {
        return Ok(Vec::new());
    };
    let mut out: Vec<(&str, &Toml)> = table
        .as_table()
        .unwrap_or(&[])
        .iter()
        .map(|(name, value)| (name.as_str(), value))
        .collect();
    out.sort_by(|a, b| a.0.cmp(b.0));
    Ok(out)
}

impl RawConfig {
    fn from_toml(root: &Toml) -> Result<Self, TomlError> {
        root.check_known_keys(CONFIG_KEYS)?;
        let section = |key: &str| root.optional_table(key);
        let ui = match section("ui")? {
            Some(table) => RawUiConfig::from_toml(table)?,
            None => RawUiConfig::default(),
        };
        let mail = match section("mail")? {
            Some(table) => RawMailConfig::from_toml(table)?,
            None => RawMailConfig::default(),
        };
        let jmap = match section("jmap")? {
            Some(table) => Some(RawAccountFields::from_toml(table)?),
            None => None,
        };
        let mut account = Vec::new();
        for (name, table) in named_sections(root, "account")? {
            account.push((name.to_string(), RawAccountFields::from_toml(table)?));
        }
        let mut retention = Vec::new();
        for (name, table) in named_sections(root, "retention")? {
            retention.push((name.to_string(), RawRetentionPolicy::from_toml(table)?));
        }
        let spam = match section("spam")? {
            Some(table) => RawSpamConfig::from_toml(table)?,
            None => RawSpamConfig::default(),
        };
        if let Some(table) = section("theme")? {
            table.check_known_keys(THEME_KEYS)?;
        }
        Ok(RawConfig {
            ui,
            mail,
            jmap,
            account,
            retention,
            spam,
        })
    }
}

#[derive(Debug)]
struct RawSpamConfig {
    enabled: bool,
    threshold: f64,
    ham_threshold: f64,
    min_training: u32,
}

const SPAM_KEYS: &[&str] = &["enabled", "threshold", "ham_threshold", "min_training"];

impl RawSpamConfig {
    fn from_toml(table: &Toml) -> Result<Self, TomlError> {
        table.check_known_keys(SPAM_KEYS)?;
        Ok(RawSpamConfig {
            enabled: table
                .optional_bool("enabled")?
                .unwrap_or_else(default_spam_enabled),
            threshold: table
                .optional_float("threshold")?
                .unwrap_or_else(default_spam_threshold),
            ham_threshold: table
                .optional_float("ham_threshold")?
                .unwrap_or_else(default_spam_ham_threshold),
            min_training: table
                .optional_u32("min_training")?
                .unwrap_or_else(default_spam_min_training),
        })
    }
}

impl Default for RawSpamConfig {
    fn default() -> Self {
        Self {
            enabled: default_spam_enabled(),
            threshold: default_spam_threshold(),
            ham_threshold: default_spam_ham_threshold(),
            min_training: default_spam_min_training(),
        }
    }
}

#[derive(Debug)]
struct RawUiConfig {
    browser: Option<String>,
    page_size: u32,
    mouse: bool,
    sync_interval_secs: u64,
}

const UI_KEYS: &[&str] = &[
    "editor",
    "browser",
    "page_size",
    "scrolloff",
    "mouse",
    "sync_interval_secs",
];

impl RawUiConfig {
    fn from_toml(table: &Toml) -> Result<Self, TomlError> {
        table.check_known_keys(UI_KEYS)?;
        // The terminal's keys: composing is in the window now, and the
        // toolkit's list keeps the selection in view; each value is
        // still checked, not read.
        let _ = table.optional_str("editor")?;
        let _ = table.optional_usize("scrolloff")?;
        Ok(RawUiConfig {
            browser: table.optional_str("browser")?.map(str::to_string),
            page_size: table
                .optional_u32("page_size")?
                .unwrap_or_else(default_page_size),
            mouse: table.optional_bool("mouse")?.unwrap_or_else(default_mouse),
            sync_interval_secs: table
                .optional_u64("sync_interval_secs")?
                .unwrap_or_else(default_sync_interval_secs),
        })
    }
}

impl Default for RawUiConfig {
    fn default() -> Self {
        Self {
            browser: None,
            page_size: default_page_size(),
            mouse: default_mouse(),
            sync_interval_secs: default_sync_interval_secs(),
        }
    }
}

#[derive(Debug)]
struct RawMailConfig {
    archive_folder: String,
    deleted_folder: String,
    archive_mailbox_id: Option<String>,
    deleted_mailbox_id: Option<String>,
    reply_from: Option<String>,
    rules_mailbox_regex: String,
    my_email_regex: String,
}

const MAIL_KEYS: &[&str] = &[
    "archive_folder",
    "deleted_folder",
    "archive_mailbox_id",
    "deleted_mailbox_id",
    "reply_from",
    "rules_mailbox_regex",
    "my_email_regex",
];

impl RawMailConfig {
    fn from_toml(table: &Toml) -> Result<Self, TomlError> {
        table.check_known_keys(MAIL_KEYS)?;
        Ok(RawMailConfig {
            archive_folder: table
                .optional_str("archive_folder")?
                .map(str::to_string)
                .unwrap_or_else(default_archive_folder),
            deleted_folder: table
                .optional_str("deleted_folder")?
                .map(str::to_string)
                .unwrap_or_else(default_deleted_folder),
            archive_mailbox_id: table
                .optional_str("archive_mailbox_id")?
                .map(str::to_string),
            deleted_mailbox_id: table
                .optional_str("deleted_mailbox_id")?
                .map(str::to_string),
            reply_from: table.optional_str("reply_from")?.map(str::to_string),
            rules_mailbox_regex: table
                .optional_str("rules_mailbox_regex")?
                .map(str::to_string)
                .unwrap_or_else(default_rules_mailbox_regex),
            my_email_regex: table
                .optional_str("my_email_regex")?
                .map(str::to_string)
                .unwrap_or_else(default_my_email_regex),
        })
    }
}

impl Default for RawMailConfig {
    fn default() -> Self {
        Self {
            archive_folder: default_archive_folder(),
            deleted_folder: default_deleted_folder(),
            archive_mailbox_id: None,
            deleted_mailbox_id: None,
            reply_from: None,
            rules_mailbox_regex: default_rules_mailbox_regex(),
            my_email_regex: default_my_email_regex(),
        }
    }
}

#[derive(Debug)]
struct RawAccountFields {
    well_known_url: Option<String>,
    username: Option<String>,
    password_command: Option<String>,
    secret: Option<String>,
    /// The retired key is recognised so its refusal can say what replaced it.
    password_file: bool,
}

const ACCOUNT_KEYS: &[&str] = &["well_known_url", "username", "password_command", "secret"];

impl RawAccountFields {
    fn from_toml(table: &Toml) -> Result<Self, TomlError> {
        // The retired key is recognised, so its refusal can say what replaced
        // it, and not advertised: an unknown key is named against the four.
        if let Some(key) = table
            .unknown_keys(ACCOUNT_KEYS)
            .iter()
            .find(|key| key.as_str() != "password_file")
        {
            return Err(TomlError::unknown_field(key, ACCOUNT_KEYS));
        }
        Ok(RawAccountFields {
            well_known_url: table.optional_str("well_known_url")?.map(str::to_string),
            username: table.optional_str("username")?.map(str::to_string),
            password_command: table.optional_str("password_command")?.map(str::to_string),
            secret: table.optional_str("secret")?.map(str::to_string),
            password_file: table.get("password_file").is_some(),
        })
    }
}

#[derive(Debug)]
struct RawRetentionPolicy {
    folder: Option<String>,
    days: Option<u32>,
}

const RETENTION_KEYS: &[&str] = &["folder", "days"];

impl RawRetentionPolicy {
    fn from_toml(table: &Toml) -> Result<Self, TomlError> {
        table.check_known_keys(RETENTION_KEYS)?;
        Ok(RawRetentionPolicy {
            folder: table.optional_str("folder")?.map(str::to_string),
            days: table.optional_u32("days")?,
        })
    }
}

fn default_page_size() -> u32 {
    500
}

fn default_mouse() -> bool {
    true
}

fn default_sync_interval_secs() -> u64 {
    60
}

fn default_archive_folder() -> String {
    "archive".to_string()
}

fn default_deleted_folder() -> String {
    "trash".to_string()
}

fn default_rules_mailbox_regex() -> String {
    "^INBOX$".to_string()
}

fn default_my_email_regex() -> String {
    "^$".to_string()
}

fn default_spam_enabled() -> bool {
    true
}

fn default_spam_threshold() -> f64 {
    0.9
}

fn default_spam_ham_threshold() -> f64 {
    0.2
}

fn default_spam_min_training() -> u32 {
    20
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let contents = fs::read_to_string(path).map_err(ConfigError::Io)?;
        Self::parse(&contents)
    }

    fn parse(contents: &str) -> Result<Self, ConfigError> {
        let document = td_toml::parse(contents).map_err(|e| ConfigError::Parse(e.to_string()))?;
        let raw = RawConfig::from_toml(&document).map_err(|e| ConfigError::Parse(e.to_string()))?;

        // Compiled here, and carried compiled: a refused pattern is a
        // configuration error naming the pattern and the reason, and nothing
        // downstream has to compile it a second time.
        let rules_mailbox_regex =
            UserRegex::compile(&raw.mail.rules_mailbox_regex).map_err(|e| {
                ConfigError::Parse(format!(
                    "invalid regex '{}' for rules_mailbox_regex: {}",
                    raw.mail.rules_mailbox_regex, e
                ))
            })?;
        let my_email_regex = UserRegex::compile(&raw.mail.my_email_regex).map_err(|e| {
            ConfigError::Parse(format!(
                "invalid regex '{}' for my_email_regex: {}",
                raw.mail.my_email_regex, e
            ))
        })?;

        let mut retention_policies = Vec::new();
        for (name, policy) in raw.retention {
            let folder = policy.folder.ok_or_else(|| {
                ConfigError::Parse(format!("missing folder in [retention.{}]", name))
            })?;
            let days = policy.days.ok_or_else(|| {
                ConfigError::Parse(format!("missing days in [retention.{}]", name))
            })?;
            if days == 0 {
                return Err(ConfigError::Parse(format!(
                    "days must be greater than 0 in [retention.{}]",
                    name
                )));
            }
            retention_policies.push(RetentionPolicyConfig { name, folder, days });
        }

        let mut accounts = Vec::new();
        for (name, account) in raw.account {
            let account_name = name.clone();
            accounts.push(AccountConfig {
                name,
                well_known_url: require_field(
                    account.well_known_url,
                    &format!("missing well_known_url in [account.{}]", account_name),
                )?,
                username: require_field(
                    account.username,
                    &format!("missing username in [account.{}]", account_name),
                )?,
                password: password_source(
                    account.password_command,
                    account.secret,
                    account.password_file,
                    &account_name,
                    &format!("[account.{}]", account_name),
                )?,
            });
        }

        if accounts.is_empty() {
            let jmap = raw.jmap.ok_or_else(|| {
                ConfigError::Parse(
                    "missing well_known_url (in [jmap] or [account.NAME])".to_string(),
                )
            })?;
            accounts.push(AccountConfig {
                name: "default".to_string(),
                well_known_url: require_field(
                    jmap.well_known_url,
                    "missing well_known_url (in [jmap] or [account.NAME])",
                )?,
                username: require_field(
                    jmap.username,
                    "missing username (in [jmap] or [account.NAME])",
                )?,
                password: password_source(
                    jmap.password_command,
                    jmap.secret,
                    jmap.password_file,
                    "default",
                    "[jmap]",
                )?,
            });
        }

        for (field, value) in [
            ("spam.threshold", raw.spam.threshold),
            ("spam.ham_threshold", raw.spam.ham_threshold),
        ] {
            if !(0.0..=1.0).contains(&value) {
                return Err(ConfigError::Parse(format!(
                    "{} must be between 0.0 and 1.0 (got {})",
                    field, value
                )));
            }
        }
        if raw.spam.ham_threshold > raw.spam.threshold {
            return Err(ConfigError::Parse(format!(
                "spam.ham_threshold ({}) must not exceed spam.threshold ({})",
                raw.spam.ham_threshold, raw.spam.threshold
            )));
        }

        Ok(Config {
            accounts,
            ui: UiConfig {
                browser: raw.ui.browser,
                page_size: raw.ui.page_size,
                mouse: raw.ui.mouse,
                sync_interval_secs: if raw.ui.sync_interval_secs == 0 {
                    None
                } else {
                    Some(raw.ui.sync_interval_secs)
                },
            },
            mail: MailConfig {
                archive_folder: raw.mail.archive_folder,
                deleted_folder: raw.mail.deleted_folder,
                archive_mailbox_id: raw.mail.archive_mailbox_id,
                deleted_mailbox_id: raw.mail.deleted_mailbox_id,
                reply_from: raw.mail.reply_from,
                rules_mailbox_regex,
                my_email_regex,
                retention_policies,
            },
            spam: SpamConfig {
                enabled: raw.spam.enabled,
                threshold: raw.spam.threshold,
                ham_threshold: raw.spam.ham_threshold,
                min_training: raw.spam.min_training,
            },
        })
    }
}

fn require_field(value: Option<String>, err: &str) -> Result<String, ConfigError> {
    value.ok_or_else(|| ConfigError::Parse(err.to_string()))
}

/// RFC 2606's reserved names, and RFC 6761's `example` top-level domain: a
/// host under one names no server, so an account that points at one is a
/// placeholder to fill in, not a connection to try.
fn reserved_example_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    ["example", "example.com", "example.net", "example.org"]
        .iter()
        .any(|reserved| {
            host == *reserved
                || host
                    .strip_suffix(reserved)
                    .is_some_and(|label| label.ends_with('.'))
        })
}

/// The authority of an http(s) URL: what lies between the scheme and the
/// path.
fn url_authority(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    rest.split(['/', '?', '#']).next()
}

/// The host of an http(s) URL, without userinfo or port; an IPv6 literal
/// keeps its brackets.
fn url_host(url: &str) -> Option<&str> {
    let host_port = url_authority(url)?.rsplit('@').next()?;
    let host = if host_port.starts_with('[') {
        // An IPv6 literal: hex digits, colons and a dotted IPv4 tail.
        host_port
            .find(']')
            .and_then(|end| host_port.get(..=end))
            .filter(|host| {
                host.get(1..host.len().saturating_sub(1))
                    .is_some_and(|inner| {
                        inner.contains(':')
                            && inner
                                .bytes()
                                .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
                    })
            })?
    } else {
        host_port.split(':').next()?
    };
    Some(host).filter(|host| !host.is_empty() && *host != "[]")
}

/// Whether what follows an http(s) URL's host is nothing or a port from 1
/// to 65535.
fn url_port_valid(url: &str) -> bool {
    let (Some(authority), Some(host)) = (url_authority(url), url_host(url)) else {
        return false;
    };
    let Some((_, after)) = authority
        .rsplit('@')
        .next()
        .and_then(|h| h.split_once(host))
    else {
        return false;
    };
    match after.strip_prefix(':') {
        None => after.is_empty(),
        Some(port) => {
            port.len() <= 5
                && port.bytes().all(|b| b.is_ascii_digit())
                && port
                    .parse::<u32>()
                    .is_ok_and(|port| (1..=65535).contains(&port))
        }
    }
}

impl AccountConfig {
    /// Whether the account's server is a reserved example name, as the
    /// account td-firstboot provisions is until it is set up.
    pub fn placeholder(&self) -> bool {
        url_host(&self.well_known_url).is_some_and(reserved_example_host)
    }
}

/// The most bytes a server or address typed into the setup form may hold.
const SETUP_FIELD_MAX: usize = 512;

/// The discovery URL for what the setup form was given: a full
/// `https://` URL as typed, or a bare host as its RFC 8620 well-known URL.
pub fn discovery_url(server: &str) -> Result<String, String> {
    let server = server.trim();
    if server.is_empty() {
        return Err("type the server: a host, or its https:// discovery URL".into());
    }
    if server.len() > SETUP_FIELD_MAX || server.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err("the server holds a space, a control character or too many bytes".into());
    }
    let url = if server.starts_with("https://") {
        server.to_string()
    } else if server.contains("://") {
        return Err("the server's URL must be https://".into());
    } else if server.contains(['/', '?', '#', '@']) {
        return Err("a bare server is a host name only; give a full https:// URL otherwise".into());
    } else {
        format!("https://{server}/.well-known/jmap")
    };
    // The URL is written to the configuration and the log: a password in
    // it would be kept in both.
    if url_authority(&url).is_some_and(|authority| authority.contains('@')) {
        return Err("the server's URL holds a user name; the address is typed next".into());
    }
    match url_host(&url) {
        None => Err("the server's URL names no host".into()),
        Some(host) if reserved_example_host(host) => {
            Err(format!("{host} is a reserved example name, not a server"))
        }
        Some(_) if !url_port_valid(&url) => {
            Err("the server's port is not a number from 1 to 65535".into())
        }
        Some(_) => Ok(url),
    }
}

/// The address the setup form was given, checked as a value to write.
pub fn setup_username(username: &str) -> Result<String, String> {
    let username = username.trim();
    if username.is_empty() {
        return Err("type the account's address or user name".into());
    }
    if username.len() > SETUP_FIELD_MAX || username.chars().any(char::is_control) {
        return Err("the address holds a control character or too many bytes".into());
    }
    Ok(username.to_string())
}

/// Whether `line` is the table header `[account.NAME]` as TOML lets it be
/// written: spaces inside the brackets, the name quoted, a comment after.
fn is_account_header(line: &str, name: &str) -> bool {
    let Some((inner, after)) = line
        .trim()
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
    else {
        return false;
    };
    let after = after.trim();
    if inner.starts_with('[') || !(after.is_empty() || after.starts_with('#')) {
        return false;
    }
    let Some((table, key)) = inner.split_once('.') else {
        return false;
    };
    let key = key.trim();
    table.trim() == "account"
        && (key == name || key.strip_prefix('"').and_then(|k| k.strip_suffix('"')) == Some(name))
}

/// `contents` with `[account.NAME]`'s `well_known_url` and `username`
/// replaced and nothing else changed. Each key must be on one line of its
/// own in that section, once, and the section written once, or the file
/// is not one this edits.
fn replace_account_server(
    contents: &str,
    name: &str,
    url: &str,
    username: &str,
) -> Result<String, String> {
    let mut out = String::with_capacity(contents.len() + url.len() + username.len());
    let mut sections = 0;
    let mut in_section = false;
    let mut replaced = [0usize; 2];
    for line in contents.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = is_account_header(trimmed, name);
            sections += usize::from(in_section);
        }
        let key = match trimmed.split_once('=') {
            Some((key, _)) if in_section => key.trim(),
            _ => "",
        };
        let (index, value) = match key {
            "well_known_url" => (0, url),
            "username" => (1, username),
            _ => {
                out.push_str(line);
                continue;
            }
        };
        if let Some(count) = replaced.get_mut(index) {
            *count += 1;
        }
        let ending = if line.ends_with("\r\n") {
            "\r\n"
        } else if line.ends_with('\n') {
            "\n"
        } else {
            ""
        };
        // td-toml's own literal: it escapes what a basic string must and
        // parses back to the same value.
        let literal = Toml::Str(value.to_owned());
        out.push_str(&format!("{key} = {literal}{ending}"));
    }
    if sections != 1 || replaced != [1, 1] {
        return Err(format!(
            "the configuration does not hold [account.{name}] with one well_known_url line and one username line"
        ));
    }
    Ok(out)
}

/// Whether the configuration at `path` holds account `name` where the
/// setup form can change it, and why not when it does not.
pub fn editable(path: &Path, name: &str) -> Result<(), String> {
    let contents =
        fs::read_to_string(path).map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    replace_account_server(&contents, name, "", "").map(|_| ())
}

/// Sets the server and address of account `name` in the configuration at
/// `path`, the rest of the file as it was. `current` is the account as
/// td-mail runs it: the file must still hold its server and address, so
/// an edit made behind td-mail's back is never overwritten. The new text
/// is parsed whole, and the account checked in it, before it replaces
/// the old file.
pub fn set_up_account(
    path: &Path,
    current: &AccountConfig,
    server: &str,
    username: &str,
) -> Result<AccountConfig, String> {
    let url = discovery_url(server)?;
    let username = setup_username(username)?;
    // A link is followed, so its target is what is replaced.
    let path =
        fs::canonicalize(path).map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    // Held from the read to the rename: a second setup at once waits, then
    // reads the first one's file, so neither renames the other's sibling.
    let dir = path.parent().unwrap_or(Path::new("."));
    let lock = fs::File::open(dir).map_err(|e| format!("cannot open {}: {}", dir.display(), e))?;
    lock.lock()
        .map_err(|e| format!("cannot lock {}: {}", dir.display(), e))?;
    let contents =
        fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    let name = current.name.as_str();
    let on_disk = Config::parse(&contents).map_err(|e| e.to_string())?;
    if !on_disk.accounts.iter().any(|account| {
        account.name == name
            && account.well_known_url == current.well_known_url
            && account.username == current.username
    }) {
        return Err(format!(
            "[account.{name}] in {} has changed since td-mail read it; it is left as it is",
            path.display()
        ));
    }
    let updated = replace_account_server(&contents, name, &url, &username)?;
    let config = Config::parse(&updated).map_err(|e| e.to_string())?;
    let account = config
        .accounts
        .into_iter()
        .find(|account| account.name == name)
        .filter(|account| account.well_known_url == url && account.username == username)
        .ok_or_else(|| format!("[account.{name}] did not parse back as written"))?;
    write_replacing(&path, updated.as_bytes())
        .map_err(|e| format!("cannot write {}: {}", path.display(), e))?;
    Ok(account)
}

/// Replaces the file at `path` whole: a sibling is written, flushed and
/// renamed over it with the old file's permissions, so a crash leaves the
/// old file or the new one, never part of either. The sibling's name is
/// fixed, so one a crash left is the one removed next time.
fn write_replacing(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let file_name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("the configuration path names no file"))?;
    let mut temporary = file_name.to_os_string();
    temporary.push(".setup");
    let temporary = dir.join(temporary);
    let permissions = fs::metadata(path)?.permissions();
    match fs::remove_file(&temporary) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let written = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.set_permissions(permissions)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
        return written;
    }
    // Renamed is replaced: a directory that cannot be synced leaves the
    // new file in place, and saying otherwise would leave the window on
    // an account the file no longer holds.
    if let Err(e) = fs::File::open(dir).and_then(|dir| dir.sync_all()) {
        crate::log_warn!(
            "[Setup] {} renamed, its directory not synced: {}",
            path.display(),
            e
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jmap_config(extra_ui: &str) -> String {
        format!(
            r#"
{extra_ui}
[jmap]
well_known_url = "https://mx.example.com/.well-known/jmap"
username = "user@example.com"
password_command = "pass show email/example.com"
"#
        )
    }

    #[test]
    fn test_parse_legacy_jmap_config() {
        let config = Config::parse(
            r#"
[jmap]
well_known_url = "https://mx.example.com/.well-known/jmap"
username = "user@example.com"
password_command = "pass show email/example.com"

[ui]
page_size = 25
scrolloff = 3
"#,
        )
        .unwrap();

        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].name, "default");
        assert_eq!(config.ui.page_size, 25);
        assert_eq!(config.ui.sync_interval_secs, Some(60));
        assert!(config.ui.mouse);
        assert_eq!(config.mail.rules_mailbox_regex.as_str(), "^INBOX$");
        // Spam defaults when no [spam] section is present.
        assert!(config.spam.enabled);
        assert_eq!(config.spam.threshold, 0.9);
        assert_eq!(config.spam.ham_threshold, 0.2);
        assert_eq!(config.spam.min_training, 20);
    }

    #[test]
    fn test_parse_spam_overrides() {
        let config = Config::parse(&jmap_config(
            r#"
[spam]
enabled = false
threshold = 0.95
ham_threshold = 0.1
min_training = 50
"#,
        ))
        .unwrap();
        assert!(!config.spam.enabled);
        assert_eq!(config.spam.threshold, 0.95);
        assert_eq!(config.spam.ham_threshold, 0.1);
        assert_eq!(config.spam.min_training, 50);
    }

    #[test]
    fn test_parse_spam_rejects_ham_above_threshold() {
        let err = Config::parse(&jmap_config(
            r#"
[spam]
threshold = 0.5
ham_threshold = 0.8
"#,
        ))
        .unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn test_parse_spam_rejects_out_of_range_threshold() {
        let err = Config::parse(&jmap_config(
            r#"
[spam]
threshold = 1.5
"#,
        ))
        .unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn test_parse_multi_account_config() {
        let config = Config::parse(
            r#"
[ui]
editor = "nvim"
page_size = 100
scrolloff = 2

[account.personal]
well_known_url = "https://mx.example.com/.well-known/jmap"
username = "user@example.com"
password_command = "pass show email/example.com"

[account.work]
well_known_url = "https://mx.work.com/.well-known/jmap"
username = "user@work.com"
password_command = "pass show email/work.com"
"#,
        )
        .unwrap();

        assert_eq!(config.accounts.len(), 2);
        assert_eq!(config.accounts[0].name, "personal");
        assert_eq!(config.accounts[1].name, "work");
        // `editor` is accepted and ignored; its value is still typed.
        let bad = jmap_config("[ui]\neditor = 1");
        assert!(matches!(Config::parse(&bad), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn test_defaults_and_sync_interval_zero() {
        let config = Config::parse(&jmap_config("[ui]\nsync_interval_secs = 0")).unwrap();
        assert_eq!(config.ui.page_size, 500);
        assert_eq!(config.ui.sync_interval_secs, None);
    }

    #[test]
    fn test_unknown_section_or_key_errors() {
        let err = Config::parse(
            r#"
[bogus]
foo = "bar"

[jmap]
well_known_url = "https://mx.example.com/.well-known/jmap"
username = "user@example.com"
password_command = "pass show email/example.com"
"#,
        )
        .unwrap_err();
        match err {
            ConfigError::Parse(msg) => assert!(msg.contains("unknown field"), "got: {}", msg),
            _ => panic!("expected parse error"),
        }
    }

    #[test]
    fn test_missing_required_account_fields() {
        let err = Config::parse(
            r#"
[account.broken]
well_known_url = "https://mx.example.com/.well-known/jmap"
username = "user@example.com"
"#,
        )
        .unwrap_err();
        match err {
            ConfigError::Parse(msg) => {
                assert!(msg.contains("missing password_command"), "got: {}", msg)
            }
            _ => panic!("expected parse error"),
        }
    }

    #[test]
    fn test_invalid_regex_validation() {
        let err = Config::parse(&jmap_config("[mail]\nrules_mailbox_regex = \"(\"")).unwrap_err();
        match err {
            ConfigError::Parse(msg) => {
                assert!(msg.contains("invalid regex"), "got: {}", msg);
                assert!(msg.contains("rules_mailbox_regex"), "got: {}", msg);
            }
            _ => panic!("expected parse error"),
        }
    }

    #[test]
    fn test_retention_policies() {
        let config = Config::parse(
            r#"
[retention.archive]
folder = "Archive"
days = 365

[retention.trash]
folder = "Trash"
days = 30

[jmap]
well_known_url = "https://mx.example.com/.well-known/jmap"
username = "user@example.com"
password_command = "pass show email/example.com"
"#,
        )
        .unwrap();

        assert_eq!(config.mail.retention_policies.len(), 2);
        assert_eq!(config.mail.retention_policies[0].name, "archive");
        assert_eq!(config.mail.retention_policies[1].days, 30);
    }

    #[test]
    fn test_mailbox_id_overrides() {
        let config = Config::parse(
            r#"
[mail]
archive_mailbox_id = "mbox-archive"
deleted_mailbox_id = "mbox-trash"

[jmap]
well_known_url = "https://mx.example.com/.well-known/jmap"
username = "user@example.com"
password_command = "pass show email/example.com"
"#,
        )
        .unwrap();

        assert_eq!(
            config.mail.archive_mailbox_id.as_deref(),
            Some("mbox-archive")
        );
        assert_eq!(
            config.mail.deleted_mailbox_id.as_deref(),
            Some("mbox-trash")
        );
    }

    /// The terminal's `scrolloff` and `[theme]` still load, ignored, so a
    /// configuration written for it is not refused; a key the terminal
    /// never had is.
    #[test]
    fn the_terminals_theme_and_scrolloff_are_accepted_and_ignored() {
        let config = Config::parse(&jmap_config(
            r##"[ui]
scrolloff = 3

[theme]
bg = "#002b36"
header_fg = "not even a colour"
"##,
        ))
        .unwrap();
        assert_eq!(config.ui.page_size, 500);
        assert!(Config::parse(&jmap_config("[ui]\nscrolloff = \"three\"")).is_err());
        let err = Config::parse(&jmap_config("[theme]\ncursor = \"#ffffff\"")).unwrap_err();
        match err {
            ConfigError::Parse(msg) => assert!(msg.contains("cursor"), "got: {}", msg),
            _ => panic!("expected parse error"),
        }
    }

    #[test]
    fn test_reply_from_override() {
        let config = Config::parse(
            r#"
[mail]
reply_from = "Example User <user@example.com>"

[jmap]
well_known_url = "https://mx.example.com/.well-known/jmap"
username = "user@example.com"
password_command = "pass show email/example.com"
"#,
        )
        .unwrap();

        assert_eq!(
            config.mail.reply_from.as_deref(),
            Some("Example User <user@example.com>")
        );
    }

    #[test]
    fn the_portal_is_one_of_two_password_sources() {
        let config = Config::parse(
            r#"
[account.td]
well_known_url = "https://mx.example.com/.well-known/jmap"
username = "user@example.com"
secret = "portal"
"#,
        )
        .unwrap();
        assert_eq!(
            config.accounts[0].password,
            PasswordSource::Portal("td".to_string())
        );
        let command = Config::parse(&jmap_config("")).unwrap();
        assert_eq!(
            command.accounts[0].password,
            PasswordSource::Command("pass show email/example.com".to_string())
        );
        // The legacy section has no name of its own; its credential is
        // mail/default.
        let legacy = Config::parse(
            "[jmap]\nwell_known_url = \"https://mx.example.com/.well-known/jmap\"\nusername = \"u@example.com\"\nsecret = \"portal\"\n",
        )
        .unwrap();
        assert_eq!(
            legacy.accounts[0].password,
            PasswordSource::Portal("default".to_string())
        );
        let jmap = Config::parse(
            "[jmap]\nwell_known_url = \"https://mx.example.com/.well-known/jmap\"\nusername = \"u@example.com\"\npassword_file = \"/p\"\n",
        )
        .unwrap_err();
        match jmap {
            ConfigError::Parse(msg) => assert!(
                msg.contains("password_file in [jmap] is no longer a password source: submit the password from the human session with `td-secret set mail/default < file`"),
                "got: {}",
                msg
            ),
            _ => panic!("expected parse error"),
        }
        for (body, needle) in [
            (
                "well_known_url = \"https://mx.example.com/.well-known/jmap\"\nusername = \"u@example.com\"\n",
                "missing password_command or secret",
            ),
            (
                "well_known_url = \"https://mx.example.com/.well-known/jmap\"\nusername = \"u@example.com\"\nsecret = \"portal\"\nbogus = 1\n",
                "unknown field `bogus`, expected one of `well_known_url`, `username`, `password_command`, `secret`",
            ),
            (
                "well_known_url = \"https://mx.example.com/.well-known/jmap\"\nusername = \"u@example.com\"\npassword_command = \"pass\"\nsecret = \"portal\"\n",
                "both password_command and secret",
            ),
            (
                "well_known_url = \"https://mx.example.com/.well-known/jmap\"\nusername = \"u@example.com\"\nsecret = \"hunter2\"\n",
                "secret in [account.td] takes only \"portal\"",
            ),
            (
                "well_known_url = \"https://mx.example.com/.well-known/jmap\"\nusername = \"u@example.com\"\npassword_file = \"/home/td/.config/td-mail/password\"\n",
                "submit the password from the human session with `td-secret set mail/td < file`, press Ctrl+Alt+Esc then W, verify the target and touch the token, and set secret = \"portal\"",
            ),
        ] {
            let err = Config::parse(&format!("[account.td]\n{}", body)).unwrap_err();
            match err {
                ConfigError::Parse(msg) => {
                    assert!(msg.contains(needle), "got: {}", msg);
                    // A password typed into `secret` is the obvious mistake;
                    // the refusal does not repeat it.
                    assert!(!msg.contains("hunter2"), "the value is echoed: {}", msg);
                }
                _ => panic!("expected parse error"),
            }
        }
    }

    /// td-secret refuses a name outside `[A-Za-z0-9_-]{1,64}`; the account
    /// name is the credential name, so the refusal is a configuration error.
    #[test]
    fn a_name_the_portal_cannot_store_is_a_configuration_error() {
        let account = |name: &str| {
            format!(
                "[account.{}]\nwell_known_url = \"https://mx.example.com/.well-known/jmap\"\nusername = \"u@example.com\"\nsecret = \"portal\"\n",
                name
            )
        };
        let longest = "a".repeat(64);
        let config = Config::parse(&account(&longest)).unwrap();
        assert_eq!(config.accounts[0].password, PasswordSource::Portal(longest));
        for name in ["a".repeat(65), "\"a b\"".to_string(), "\"\"".to_string()] {
            let err = Config::parse(&account(&name)).unwrap_err();
            match err {
                ConfigError::Parse(msg) => assert!(
                    msg.contains(
                        "the portal cannot name this account; a credential name is 1 to 64 bytes"
                    ),
                    "{}: got: {}",
                    name,
                    msg
                ),
                _ => panic!("expected parse error for {}", name),
            }
        }
        // The rule is td-secret's, byte for byte.
        assert!(portal_name("a-b_C9", "[account.a-b_C9]").is_ok());
        assert!(portal_name("", "[jmap]").is_err());
        assert!(portal_name("a.b", "[account.a]").is_err());
        assert!(portal_name("é", "[account.é]").is_err());
    }

    /// `MAIL_CONFIG` from td's `td-firstboot/src/main.rs`, copied byte for
    /// byte: the file a td image provisions at `~/.config/td-mail/config.toml`
    /// on first boot.
    const FIRSTBOOT_CONFIG: &str = "\
# td-mail. Provisioned on first boot; edit freely, it is never rewritten.
# Paths are as the application sees them inside its jail. The client reads
# this file when it starts.

[account.main]
well_known_url = \"https://mail.example.com/.well-known/jmap\"
username = \"you@example.com\"
secret = \"portal\"
";

    /// The parser that reads that file is now td's own TOML, not serde's, so
    /// this is the parity check: the shipped text still yields the shipped
    /// account. A divergence on either side is red here rather than a first
    /// boot into a client that cannot read its own configuration.
    #[test]
    fn test_parse_firstboot_provisioned_config() {
        let config = Config::parse(FIRSTBOOT_CONFIG).expect("the shipped config parses");
        assert_eq!(config.accounts.len(), 1);
        let account = &config.accounts[0];
        assert_eq!(account.name, "main");
        assert_eq!(
            account.well_known_url,
            "https://mail.example.com/.well-known/jmap"
        );
        assert_eq!(account.username, "you@example.com");
        assert_eq!(account.password, PasswordSource::Portal("main".to_string()));
        assert!(account.placeholder());
    }

    #[test]
    fn a_reserved_example_server_is_a_placeholder_and_no_other_is() {
        let account = |url: &str| AccountConfig {
            name: "a".to_string(),
            well_known_url: url.to_string(),
            username: "u@example.com".to_string(),
            password: PasswordSource::Portal("a".to_string()),
        };
        for url in [
            "https://mail.example.com/.well-known/jmap",
            "https://EXAMPLE.org./x",
            "http://u@a.b.example.net:8443/",
            "https://host.example/",
        ] {
            assert!(account(url).placeholder(), "{url}");
        }
        for url in [
            "https://api.fastmail.com/.well-known/jmap",
            "https://myexample.com/",
            "https://example.com.evil.org/",
            "https://[2001:db8::1]:8443/",
            "",
            "example.com",
        ] {
            assert!(!account(url).placeholder(), "{url}");
        }
        assert_eq!(url_host("https://[::1]:8443/x"), Some("[::1]"));
        assert_eq!(url_host("https://[]/"), None);
        assert_eq!(url_host("https://[::1/"), None);
    }

    #[test]
    fn the_setup_form_takes_a_host_or_an_https_discovery_url() {
        assert_eq!(
            discovery_url(" api.fastmail.com ").unwrap(),
            "https://api.fastmail.com/.well-known/jmap"
        );
        assert_eq!(
            discovery_url("https://mx.td.dev/jmap/session").unwrap(),
            "https://mx.td.dev/jmap/session"
        );
        assert_eq!(
            discovery_url("https://[2001:db8::1]:8443/.well-known/jmap").unwrap(),
            "https://[2001:db8::1]:8443/.well-known/jmap"
        );
        for refused in [
            "",
            "http://mx.td.dev/",
            "ftp://x",
            "mx.td.dev/path",
            "a b",
            "https://",
            "https://[]/",
            "https://me:hunter2@mx.td.dev/",
            "https://mx.td.dev:abc/",
            "https://mx.td.dev:70000/",
            "https://mx.td.dev:0/",
            "https://mx.td.dev:/",
            "mx.td.dev:99999",
            "mail.example.com",
            "https://x.example.org/",
        ] {
            assert!(discovery_url(refused).is_err(), "{refused}");
        }
        assert_eq!(
            discovery_url("https://mx.td.dev:8443/jmap").unwrap(),
            "https://mx.td.dev:8443/jmap"
        );
        assert_eq!(
            discovery_url("mx.td.dev:8443").unwrap(),
            "https://mx.td.dev:8443/.well-known/jmap"
        );
        for (input, reason) in [
            ("https://", "names no host"),
            ("https://[]/", "names no host"),
            ("https://[::1/", "names no host"),
            ("https://[foo]/", "names no host"),
            ("https://[mail.example.com]/", "names no host"),
            ("https://mx.td.dev:abc/", "port"),
        ] {
            let err = discovery_url(input).unwrap_err();
            assert!(err.contains(reason), "{input}: {err}");
        }
        assert_eq!(setup_username(" me@td.dev ").unwrap(), "me@td.dev");
        assert!(setup_username("  ").is_err());
        assert!(setup_username("a\nb").is_err());
    }

    fn firstboot_account() -> AccountConfig {
        Config::parse(FIRSTBOOT_CONFIG).unwrap().accounts.remove(0)
    }

    #[test]
    fn setting_up_an_account_replaces_two_values_and_nothing_else() {
        let dir = crate::testing::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let text = format!("{FIRSTBOOT_CONFIG}\n[ui]\npage_size = 50\n");
        std::fs::write(&path, &text).unwrap();
        // A sibling a crash left behind is not in the way.
        std::fs::write(dir.path().join("config.toml.setup"), "stale").unwrap();
        let placeholder = firstboot_account();
        let account = set_up_account(&path, &placeholder, "mx.td.dev", "me\"\\@td.dev").unwrap();
        assert_eq!(account.well_known_url, "https://mx.td.dev/.well-known/jmap");
        assert_eq!(account.username, "me\"\\@td.dev");
        assert_eq!(account.password, PasswordSource::Portal("main".to_string()));
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            written,
            text.replace(
                "https://mail.example.com/.well-known/jmap",
                "https://mx.td.dev/.well-known/jmap"
            )
            .replace("\"you@example.com\"", "\"me\\\"\\\\@td.dev\"")
        );
        assert_eq!(Config::load(&path).unwrap().ui.page_size, 50);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

        // What td-mail no longer runs is not what the file holds: refused,
        // and the file is as it was.
        let err = set_up_account(&path, &placeholder, "other.td.dev", "x@td.dev").unwrap_err();
        assert!(err.contains("has changed since td-mail read it"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
        let mut absent = account.clone();
        absent.name = "absent".to_string();
        let err = set_up_account(&path, &absent, "mx.td.dev", "x@td.dev").unwrap_err();
        assert!(err.contains("has changed since td-mail read it"), "{err}");

        // An account set up with a typo is set up again from the window.
        let fixed = set_up_account(&path, &account, "mx2.td.dev", "me@td.dev").unwrap();
        assert_eq!(fixed.well_known_url, "https://mx2.td.dev/.well-known/jmap");
        assert_eq!(
            Config::load(&path).unwrap().accounts[0].username,
            "me@td.dev"
        );
    }

    /// Setups at once are taken in turn: one is written, every other
    /// finds the file changed, and the file is always whole.
    #[test]
    fn setups_at_once_are_taken_in_turn() {
        let dir = crate::testing::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for _ in 0..20 {
            std::fs::write(&path, FIRSTBOOT_CONFIG).unwrap();
            let threads: Vec<_> = (0..4)
                .map(|n| {
                    let path = path.clone();
                    std::thread::spawn(move || {
                        set_up_account(
                            &path,
                            &firstboot_account(),
                            &format!("mx{n}.td.dev"),
                            "me@td.dev",
                        )
                        .is_ok()
                    })
                })
                .collect();
            let written = threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .filter(|ok| *ok)
                .count();
            assert_eq!(written, 1);
            let config = Config::load(&path).unwrap();
            assert!(config.accounts[0].well_known_url.starts_with("https://mx"));
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn a_linked_configuration_is_replaced_at_its_target() {
        let dir = crate::testing::tempdir().unwrap();
        let target = dir.path().join("real.toml");
        let link = dir.path().join("config.toml");
        std::fs::write(&target, FIRSTBOOT_CONFIG).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        set_up_account(&link, &firstboot_account(), "mx.td.dev", "me@td.dev").unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::read_to_string(&target)
            .unwrap()
            .contains("https://mx.td.dev/.well-known/jmap"));
    }

    #[test]
    fn a_file_this_cannot_edit_line_by_line_is_left_alone() {
        let twice = "[account.main]\nwell_known_url = \"https://a.example.com/\"\nwell_known_url = \"https://b.example.com/\"\nusername = \"u\"\n";
        assert!(replace_account_server(twice, "main", "https://x/", "y").is_err());
        let inline = "account.main = { well_known_url = \"https://a.example.com/\", username = \"u\", secret = \"portal\" }\n";
        assert!(replace_account_server(inline, "main", "https://x/", "y").is_err());
        let array =
            "[[account.main]]\nwell_known_url = \"https://a.example.com/\"\nusername = \"u\"\n";
        assert!(replace_account_server(array, "main", "https://x/", "y").is_err());
        let other = "[account.work]\nwell_known_url = \"https://w/\"\nusername = \"w\"\n[account.main]\nwell_known_url = \"https://a.example.com/\"\r\nusername = \"u\"\r\n";
        assert_eq!(
            replace_account_server(other, "main", "https://x/", "y").unwrap(),
            "[account.work]\nwell_known_url = \"https://w/\"\nusername = \"w\"\n[account.main]\nwell_known_url = \"https://x/\"\r\nusername = \"y\"\r\n"
        );
        // The header as TOML lets it be written.
        for header in [
            "[ account . main ]",
            "[account.\"main\"]",
            "[account.main]  # the one account",
        ] {
            let text = format!(
                "{header}\nwell_known_url = \"https://a.example.com/\"\nusername = \"u\"\n"
            );
            let replaced = replace_account_server(&text, "main", "https://x/", "y").unwrap();
            assert!(
                replaced.contains("well_known_url = \"https://x/\""),
                "{header}"
            );
        }
        assert!(!is_account_header("[account.main] trailing", "main"));

        // A value is written as td-toml's literal, so what parses back is the
        // value given: quotes and backslashes, and a control character the
        // form's own checks would have refused before it got here.
        let text = "[account.main]\nwell_known_url = \"https://a/\"\nusername = \"u\"\n";
        let tricky = "a\"b\\c\u{1}d\ne\u{7f}";
        let replaced = replace_account_server(text, "main", "https://x/", tricky).unwrap();
        assert_eq!(replaced.lines().count(), text.lines().count());
        let document = td_toml::parse(&replaced).unwrap();
        let field = |name: &str| {
            document
                .get("account")
                .and_then(|accounts| accounts.get("main"))
                .and_then(|main| main.get(name))
                .and_then(Toml::as_str)
        };
        assert_eq!(field("username"), Some(tricky));
        assert_eq!(field("well_known_url"), Some("https://x/"));

        // What the form can change is what the window offers it for: a
        // legacy [jmap] account is not.
        let dir = crate::testing::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, FIRSTBOOT_CONFIG).unwrap();
        assert!(editable(&path, "main").is_ok());
        assert!(editable(&path, "default").is_err());
        std::fs::write(
            &path,
            "[jmap]\nwell_known_url = \"https://mail.example.com/\"\nusername = \"u\"\nsecret = \"portal\"\n",
        )
        .unwrap();
        assert!(editable(&path, "default").is_err());
        assert!(editable(&dir.path().join("absent"), "main").is_err());
        assert!(!is_account_header("[account.mainx]", "main"));
    }
}
