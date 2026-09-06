use crate::regex::UserRegex;
use crate::toml::{self, Error as TomlError, Toml};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct AccountConfig {
    pub name: String,
    pub well_known_url: String,
    pub username: String,
    pub password: PasswordSource,
}

/// Where an account's password comes from. `Command` runs a shell command
/// and takes its stdout; `File` reads a file directly, which needs no shell
/// and suits a sandbox that has none. Exactly one is configured per account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasswordSource {
    Command(String),
    File(String),
}

fn password_source(
    password_command: Option<String>,
    password_file: Option<String>,
    section: &str,
) -> Result<PasswordSource, ConfigError> {
    match (password_command, password_file) {
        (Some(command), None) => Ok(PasswordSource::Command(command)),
        (None, Some(file)) => Ok(PasswordSource::File(file)),
        (Some(_), Some(_)) => Err(ConfigError::Parse(format!(
            "both password_command and password_file set in {}; choose one",
            section
        ))),
        (None, None) => Err(ConfigError::Parse(format!(
            "missing password_command or password_file in {}",
            section
        ))),
    }
}

#[derive(Debug, Clone, Default)]
pub struct Theme {
    pub bg: Option<(u8, u8, u8)>,
    pub fg: Option<(u8, u8, u8)>,
    pub bold_fg: Option<(u8, u8, u8)>,
    pub selection_bg: Option<(u8, u8, u8)>,
    pub selection_fg: Option<(u8, u8, u8)>,
    pub status_bg: Option<(u8, u8, u8)>,
    pub status_fg: Option<(u8, u8, u8)>,
    pub header_fg: Option<(u8, u8, u8)>,
}

fn parse_hex_color(s: &str, field: &str) -> Result<(u8, u8, u8), ConfigError> {
    let s = s.trim();
    if !s.starts_with('#') || s.len() != 7 {
        return Err(ConfigError::Parse(format!(
            "invalid color '{}' for theme.{}: expected #RRGGBB format",
            s, field
        )));
    }
    let r = u8::from_str_radix(&s[1..3], 16);
    let g = u8::from_str_radix(&s[3..5], 16);
    let b = u8::from_str_radix(&s[5..7], 16);
    match (r, g, b) {
        (Ok(r), Ok(g), Ok(b)) => Ok((r, g, b)),
        _ => Err(ConfigError::Parse(format!(
            "invalid hex digits in color '{}' for theme.{}",
            s, field
        ))),
    }
}

fn resolve_color(value: &Option<String>, field: &str) -> Result<Option<(u8, u8, u8)>, ConfigError> {
    match value {
        Some(s) => Ok(Some(parse_hex_color(s, field)?)),
        None => Ok(None),
    }
}

#[derive(Debug)]
pub struct Config {
    pub accounts: Vec<AccountConfig>,
    pub ui: UiConfig,
    pub mail: MailConfig,
    pub spam: SpamConfig,
    pub theme: Theme,
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
    pub editor: Option<String>,
    pub browser: Option<String>,
    pub page_size: u32,
    pub scrolloff: usize,
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

/// `[theme]`. `deny_unknown_fields`, as every raw section below is.
#[derive(Debug, Default)]
struct RawThemeConfig {
    bg: Option<String>,
    fg: Option<String>,
    bold_fg: Option<String>,
    selection_bg: Option<String>,
    selection_fg: Option<String>,
    status_bg: Option<String>,
    status_fg: Option<String>,
    header_fg: Option<String>,
}

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

impl RawThemeConfig {
    fn from_toml(table: &Toml) -> Result<Self, TomlError> {
        table.check_known_keys(THEME_KEYS)?;
        let text = |key: &str| table.optional_str(key).map(|v| v.map(str::to_string));
        Ok(RawThemeConfig {
            bg: text("bg")?,
            fg: text("fg")?,
            bold_fg: text("bold_fg")?,
            selection_bg: text("selection_bg")?,
            selection_fg: text("selection_fg")?,
            status_bg: text("status_bg")?,
            status_fg: text("status_fg")?,
            header_fg: text("header_fg")?,
        })
    }
}

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
    theme: RawThemeConfig,
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
        let theme = match section("theme")? {
            Some(table) => RawThemeConfig::from_toml(table)?,
            None => RawThemeConfig::default(),
        };
        Ok(RawConfig {
            ui,
            mail,
            jmap,
            account,
            retention,
            spam,
            theme,
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
    editor: Option<String>,
    browser: Option<String>,
    page_size: u32,
    scrolloff: usize,
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
        Ok(RawUiConfig {
            editor: table.optional_str("editor")?.map(str::to_string),
            browser: table.optional_str("browser")?.map(str::to_string),
            page_size: table
                .optional_u32("page_size")?
                .unwrap_or_else(default_page_size),
            scrolloff: table
                .optional_usize("scrolloff")?
                .unwrap_or_else(default_scrolloff),
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
            editor: None,
            browser: None,
            page_size: default_page_size(),
            scrolloff: default_scrolloff(),
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
    password_file: Option<String>,
}

const ACCOUNT_KEYS: &[&str] = &[
    "well_known_url",
    "username",
    "password_command",
    "password_file",
];

impl RawAccountFields {
    fn from_toml(table: &Toml) -> Result<Self, TomlError> {
        table.check_known_keys(ACCOUNT_KEYS)?;
        Ok(RawAccountFields {
            well_known_url: table.optional_str("well_known_url")?.map(str::to_string),
            username: table.optional_str("username")?.map(str::to_string),
            password_command: table.optional_str("password_command")?.map(str::to_string),
            password_file: table.optional_str("password_file")?.map(str::to_string),
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

fn default_scrolloff() -> usize {
    1
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
        let document = toml::parse(contents).map_err(|e| ConfigError::Parse(e.to_string()))?;
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
                    account.password_file,
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
                password: password_source(jmap.password_command, jmap.password_file, "[jmap]")?,
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

        let theme = Theme {
            bg: resolve_color(&raw.theme.bg, "bg")?,
            fg: resolve_color(&raw.theme.fg, "fg")?,
            bold_fg: resolve_color(&raw.theme.bold_fg, "bold_fg")?,
            selection_bg: resolve_color(&raw.theme.selection_bg, "selection_bg")?,
            selection_fg: resolve_color(&raw.theme.selection_fg, "selection_fg")?,
            status_bg: resolve_color(&raw.theme.status_bg, "status_bg")?,
            status_fg: resolve_color(&raw.theme.status_fg, "status_fg")?,
            header_fg: resolve_color(&raw.theme.header_fg, "header_fg")?,
        };

        Ok(Config {
            accounts,
            theme,
            ui: UiConfig {
                editor: raw.ui.editor,
                browser: raw.ui.browser,
                page_size: raw.ui.page_size,
                scrolloff: raw.ui.scrolloff,
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
        assert_eq!(config.ui.scrolloff, 3);
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
        assert_eq!(config.ui.editor.as_deref(), Some("nvim"));
        assert_eq!(config.ui.scrolloff, 2);
    }

    #[test]
    fn test_defaults_and_sync_interval_zero() {
        let config = Config::parse(&jmap_config("[ui]\nsync_interval_secs = 0")).unwrap();
        assert_eq!(config.ui.page_size, 500);
        assert_eq!(config.ui.scrolloff, 1);
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

    #[test]
    fn test_theme_defaults_all_none() {
        let config = Config::parse(&jmap_config("")).unwrap();
        assert!(config.theme.bg.is_none());
        assert!(config.theme.fg.is_none());
        assert!(config.theme.bold_fg.is_none());
        assert!(config.theme.selection_bg.is_none());
        assert!(config.theme.selection_fg.is_none());
        assert!(config.theme.status_bg.is_none());
        assert!(config.theme.status_fg.is_none());
        assert!(config.theme.header_fg.is_none());
    }

    #[test]
    fn test_theme_parses_hex_colors() {
        let config = Config::parse(&jmap_config(
            r##"[theme]
bg = "#002b36"
fg = "#839496"
bold_fg = "#93a1a1"
selection_bg = "#073642"
selection_fg = "#eee8d5"
status_bg = "#586e75"
status_fg = "#eee8d5"
header_fg = "#268bd2"
"##,
        ))
        .unwrap();
        assert_eq!(config.theme.bg, Some((0x00, 0x2b, 0x36)));
        assert_eq!(config.theme.fg, Some((0x83, 0x94, 0x96)));
        assert_eq!(config.theme.bold_fg, Some((0x93, 0xa1, 0xa1)));
        assert_eq!(config.theme.selection_bg, Some((0x07, 0x36, 0x42)));
        assert_eq!(config.theme.selection_fg, Some((0xee, 0xe8, 0xd5)));
        assert_eq!(config.theme.status_bg, Some((0x58, 0x6e, 0x75)));
        assert_eq!(config.theme.status_fg, Some((0xee, 0xe8, 0xd5)));
        assert_eq!(config.theme.header_fg, Some((0x26, 0x8b, 0xd2)));
    }

    #[test]
    fn test_theme_partial_colors() {
        let config = Config::parse(&jmap_config(
            "[theme]\nbg = \"#002b36\"\nheader_fg = \"#268bd2\"",
        ))
        .unwrap();
        assert_eq!(config.theme.bg, Some((0x00, 0x2b, 0x36)));
        assert!(config.theme.fg.is_none());
        assert_eq!(config.theme.header_fg, Some((0x26, 0x8b, 0xd2)));
    }

    #[test]
    fn test_theme_invalid_hex_format() {
        let err = Config::parse(&jmap_config("[theme]\nbg = \"red\"")).unwrap_err();
        match err {
            ConfigError::Parse(msg) => {
                assert!(msg.contains("invalid color"), "got: {}", msg);
                assert!(msg.contains("theme.bg"), "got: {}", msg);
            }
            _ => panic!("expected parse error"),
        }
    }

    #[test]
    fn test_theme_invalid_hex_digits() {
        let err = Config::parse(&jmap_config("[theme]\nfg = \"#ZZZZZZ\"")).unwrap_err();
        match err {
            ConfigError::Parse(msg) => {
                assert!(msg.contains("invalid hex digits"), "got: {}", msg);
                assert!(msg.contains("theme.fg"), "got: {}", msg);
            }
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
    fn test_password_file_is_one_of_two_sources() {
        let config = Config::parse(
            r#"
[account.td]
well_known_url = "https://mx.example.com/.well-known/jmap"
username = "user@example.com"
password_file = "/home/td/.config/tmc/password"
"#,
        )
        .unwrap();
        assert_eq!(
            config.accounts[0].password,
            PasswordSource::File("/home/td/.config/tmc/password".to_string())
        );
        let command = Config::parse(&jmap_config("")).unwrap();
        assert_eq!(
            command.accounts[0].password,
            PasswordSource::Command("pass show email/example.com".to_string())
        );
        for (body, needle) in [
            (
                "well_known_url = \"https://mx.example.com/.well-known/jmap\"\nusername = \"u@example.com\"\n",
                "missing password_command or password_file",
            ),
            (
                "well_known_url = \"https://mx.example.com/.well-known/jmap\"\nusername = \"u@example.com\"\npassword_command = \"pass\"\npassword_file = \"/p\"\n",
                "both password_command and password_file",
            ),
        ] {
            let err = Config::parse(&format!("[account.td]\n{}", body)).unwrap_err();
            match err {
                ConfigError::Parse(msg) => assert!(msg.contains(needle), "got: {}", msg),
                _ => panic!("expected parse error"),
            }
        }
    }

    /// `TMC_CONFIG` from td's `td-firstboot/src/main.rs`, copied byte for
    /// byte: the file a td image provisions at `~/.config/tmc/config.toml`
    /// on first boot.
    const FIRSTBOOT_CONFIG: &str = "\
# td mail (tmc). Provisioned on first boot; edit freely, it is never rewritten.
# Paths are as the application sees them inside its jail. The client reads
# this file when it starts.

[account.main]
well_known_url = \"https://mail.example.com/.well-known/jmap\"
username = \"you@example.com\"
password_file = \"/home/td/.config/tmc/password\"
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
        assert_eq!(
            account.password,
            PasswordSource::File("/home/td/.config/tmc/password".to_string())
        );
    }
}
