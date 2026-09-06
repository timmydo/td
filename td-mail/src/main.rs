// The crate's one unsafe surface is `term_sys.rs`, which allows the lint on its
// single syscall entry point. Nothing else may.
#![deny(unsafe_code)]

#[macro_use]
mod log;
// Before the modules that use `json!`, as `log`'s macros are.
#[macro_use]
mod json;

mod b64;
mod backend;
mod cache;
// The shared module carries more of the calendar than td-mail's clock needs, and
// its `Zone::from_local_*` are conversions from a local time, not constructors.
#[allow(dead_code, clippy::wrong_self_convention)]
mod civil;
mod cli;
mod compose;
mod config;
mod html;
mod jmap;
mod keybindings;
// The shared store carries more than td-mail's five tables need.
#[allow(dead_code)]
mod kv;
// The shared module carries td-txt's whole engine; td-mail reads one adapter.
#[allow(dead_code)]
mod regex;
mod rules;
mod spam;
mod td_fetch;
// The shared module carries a terminal surface wider than td-mail's one raw mode.
#[allow(dead_code)]
mod term_sys;
#[cfg(test)]
mod testing;
// The shared module carries more of TOML than td-mail's two files need.
#[allow(dead_code)]
mod toml;
mod tui;

use config::{AccountConfig, Config, PasswordSource};
use jmap::client::{JmapClient, JmapError};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;

fn default_config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("td-mail").join("config.toml")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
            .join(".config")
            .join("td-mail")
            .join("config.toml")
    } else {
        PathBuf::from("config.toml")
    }
}

pub fn run_password_command(cmd: &str) -> Result<String, String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .map_err(|e| format!("failed to execute password command: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "password command exited with {}: {}",
            output.status, stderr
        ));
    }

    let password = String::from_utf8(output.stdout)
        .map_err(|e| format!("password command output is not valid UTF-8: {}", e))?;

    Ok(password.trim_end_matches('\n').to_string())
}

/// The credential portal's helper, where the `mail` package puts it: td-secret
/// ships beside td-mail, so `secret = "portal"` is a source inside a td jail
/// and nowhere else.
pub const PORTAL_HELPER: &str = "/app/bin/td-secret";

/// Ask the credential portal for the secret stored as mail/NAME. The helper
/// receives the secret over D-Bus and an fd and prints it. Trailing newlines
/// go as they do for a command's output, so a secret stored from a file an
/// editor wrote works. A failure names the helper and the credential, never
/// the bytes: the secret is not diagnostic text.
pub fn read_portal_credential(name: &str) -> Result<String, String> {
    read_portal_credential_from(PORTAL_HELPER, name)
}

fn read_portal_credential_from(helper: &str, name: &str) -> Result<String, String> {
    // No terminal for the helper: it takes nothing from stdin, and td-mail's
    // stdin is the screen's.
    let output = Command::new(helper)
        .args(["get", name])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("credential portal: {} did not run: {}", helper, e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        return Err(if stderr.is_empty() {
            format!("credential portal: {} get {} failed with {}", helper, name, output.status)
        } else {
            format!("credential portal: {} get {}: {}", helper, name, stderr)
        });
    }
    let mut password = String::from_utf8(output.stdout).map_err(|_| {
        format!("credential portal: {} get {}: the credential is not valid UTF-8", helper, name)
    })?;
    // Trimmed in place: one buffer holds the secret, not a second copy.
    let kept = password.trim_end_matches('\n').len();
    password.truncate(kept);
    Ok(password)
}

#[cfg(test)]
mod portal_tests {
    use super::read_portal_credential_from;
    use std::os::unix::fs::PermissionsExt;

    fn helper(dir: &std::path::Path, body: &str) -> String {
        let path = dir.join("td-secret");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// The helper is asked `get NAME`, its stdin is the null device, and its
    /// trailing newlines go; every failure names the helper and the
    /// credential and never repeats what it printed. The stubs are `/bin/sh`
    /// scripts, the one path a shebang can name.
    #[test]
    fn the_helper_is_run_as_the_portal_expects_and_its_failures_are_named() {
        let dir = crate::testing::tempdir().unwrap();
        let echo = helper(dir.path(), "printf '%s:%s:%s\\n\\n' \"$1\" \"$2\" \"$(readlink /proc/self/fd/0)\"");
        assert_eq!(read_portal_credential_from(&echo, "main").unwrap(), "get:main:/dev/null");

        let refused = helper(dir.path(), "echo 'no such credential' >&2; exit 3");
        let err = read_portal_credential_from(&refused, "main").unwrap_err();
        assert!(err.contains("get main: no such credential"), "{err}");
        assert!(err.contains(&refused), "{err}");

        let silent = helper(dir.path(), "exit 4");
        let err = read_portal_credential_from(&silent, "main").unwrap_err();
        assert!(err.contains("get main failed with exit status: 4"), "{err}");

        let bytes = helper(dir.path(), "printf '\\377'");
        let err = read_portal_credential_from(&bytes, "main").unwrap_err();
        assert!(err.contains("get main: the credential is not valid UTF-8"), "{err}");
        assert!(!err.contains('\u{fffd}'), "{err}");

        let absent = dir.path().join("missing").to_string_lossy().into_owned();
        let err = read_portal_credential_from(&absent, "main").unwrap_err();
        assert!(err.contains("did not run"), "{err}");
        assert!(err.contains(&absent), "{err}");
    }
}

pub fn read_password(source: &PasswordSource) -> Result<String, String> {
    match source {
        PasswordSource::Command(command) => run_password_command(command),
        PasswordSource::Portal(name) => read_portal_credential(name),
    }
}

pub fn connect_account(account: &AccountConfig) -> Result<JmapClient, String> {
    let password = read_password(&account.password)?;
    let (_session, client) =
        JmapClient::discover(&account.well_known_url, &account.username, &password).map_err(
            |e| match e {
                // The diagnostic already names what is missing and what
                // would answer it; a discovery prefix would only bury it.
                JmapError::NoFetchService(message) => message,
                other => format!("JMAP discovery error: {}", other),
            },
        )?;
    Ok(client)
}

fn show_log() {
    let path = log::log_path();
    if !path.exists() {
        eprintln!("No log file found at {}", path.display());
        std::process::exit(1);
    }
    let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".to_string());
    let status = Command::new(&pager).arg(&path).status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("Failed to launch pager '{}': {}", pager, e);
            std::process::exit(1);
        }
    }
}

fn print_rules() {
    let config_path = default_config_path();
    let rules_path = config_path
        .parent()
        .map(|p| p.join("rules.toml"))
        .unwrap_or_else(|| PathBuf::from("rules.toml"));

    if !rules_path.exists() {
        eprintln!("No rules file found at {}", rules_path.display());
        std::process::exit(1);
    }

    let loaded = match rules::load_rules(&rules_path) {
        Ok(rules) => rules,
        Err(e) => {
            eprintln!("Failed to load rules from {}: {}", rules_path.display(), e);
            std::process::exit(1);
        }
    };
    let custom_headers = rules::extract_custom_headers(&loaded);

    println!("Rules file: {}", rules_path.display());
    println!("Rules loaded: {}", loaded.len());
    println!("Custom headers requested: {}", custom_headers.len());
    println!();
    print!("{}", rules::format_rules_for_display(&loaded));
}

fn print_prompt(topic: &str) {
    match topic {
        "config" => {
            let config_path = default_config_path();
            print!(
                r#"I need help generating a configuration file for td-mail (Timmy's Mail Console), a terminal email client that connects via JMAP.

The config file goes at: {}

Here is the format:

```toml
[ui]
editor = "nvim"          # optional: editor for composing ($EDITOR fallback)
browser = "firefox"      # optional: browser for opening URLs ($BROWSER fallback, then xdg-open)
page_size = 100           # optional: emails per page (default 500)
scrolloff = 1             # optional: keep this many context lines above/below cursor (default 1)
mouse = true              # optional: enable mouse support (default true)
sync_interval_secs = 60   # optional: background sync interval (default 60, 0 = off)

[mail]
archive_folder = "Archive"  # optional: target folder for 'a' archive action (default "archive")
deleted_folder = "Trash"    # optional: target folder for 'd' delete action (default "trash")
rules_mailbox_regex = "^INBOX$"  # optional: auto-run rules only when mailbox name matches (default "^INBOX$")
my_email_regex = "(?i)(timmy@example\\.com|me@work\\.com)" # optional: your addresses used by rules skip_if_to_me (default "^$")

[spam]
enabled = true            # optional: score new INBOX mail with the built-in classifier (default true)
threshold = 0.9           # optional: score >= this -> verdict "spam" (default 0.9)
ham_threshold = 0.2       # optional: score <= this -> verdict "ham"; between is "unsure" (default 0.2)
min_training = 20         # optional: trained messages per class before verdicts go live (default 20)

[retention.archive]
folder = "Archive"
days = 365                  # expire mail older than 365 days in Archive when pressing X

[retention.trash]
folder = "Trash"
days = 30                   # expire mail older than 30 days in Trash when pressing X

[account.personal]
well_known_url = "https://mx.example.com/.well-known/jmap"
username = "me@example.com"
secret = "portal"

[account.work]
well_known_url = "https://mx.work.com/.well-known/jmap"
username = "me@work.com"
password_command = "pass show email/work.com"
```

Rules:
- At least one [account.NAME] section is required (or legacy [jmap] with the same three fields).
- `well_known_url`, `username`, and exactly one of `password_command` or `secret` are required per account.
- `secret = "portal"` asks td's credential portal for the secret stored as mail/NAME for [account.NAME] (mail/default for a legacy [jmap] section); it needs no shell and no file, and it works inside a td jail only, where the helper is packaged. Store the secret with `td-secret set mail/NAME < file`. NAME is 1 to 64 bytes of [A-Za-z0-9_-].
- `password_command` is a shell command that prints the password to stdout.
- Quoted strings support \", \\, \n, \t escapes.
- `scrolloff` controls how many lines of context are kept above and below the cursor in list views.
- `archive_folder` and `deleted_folder` are mailbox targets for `a` and `d` in list views.
- `rules_mailbox_regex` controls which mailbox names auto-run rules on refresh/fetch; default is `^INBOX$`.
- `my_email_regex` is matched against combined To/Cc and used by rules with `skip_if_to_me = true`.
- Both patterns are POSIX Extended Regular Expressions over bytes, with GNU `\w \W \b \B` and an optional leading `(?i)`; `\d \D \s \S` and `(?:...)` are accepted. Matching is leftmost-longest and ASCII. Lookaround, backreferences, named/comment groups, non-greedy quantifiers, `\uXXXX`/`\x..`/`\p{{...}}` and a pattern over 4 KiB are refused, and a refused pattern is a config error naming the pattern and the reason. See `td-mail --prompt=rules`.
- `[spam]` configures the built-in Bayesian classifier: it scores new INBOX mail and sets an `X-Tmc-Spam-Verdict` header that rules.toml can act on (train with `J`/`H` in the message view). See `td-mail --prompt=rules`.
- `[retention.NAME]` sections are optional folder retention policies used by `x` (preview) and `X` (expire) in mailbox view.
- Retention policy fields:
  - `folder` (required): mailbox name, role, or path (e.g. "INBOX/Alerts")
  - `days` (required): positive integer; emails older than this are deleted on `X`.

Please ask me for my email provider, username, and how I store passwords, then generate a config file.
"#,
                config_path.display()
            );
        }
        "rules" => {
            let config_path = default_config_path();
            let rules_path = config_path
                .parent()
                .map(|p| p.join("rules.toml"))
                .unwrap_or_else(|| PathBuf::from("rules.toml"));
            print!(
                r#"I need help generating a rules file for td-mail (Timmy's Mail Console), a terminal email client.

The rules file goes at: {}

Here is the format:

```toml
# Simple rule: match a header with a regex, apply actions
[[rule]]
name = "mark newsletters read"
skip_if_to_me = true
[rule.match]
header = "From"
regex = "newsletter@"
[rule.actions]
mark_read = true

# Compound conditions: all, any, not
[[rule]]
name = "flag urgent from boss"
[rule.match]
all = [
    {{ header = "From", regex = "boss@example\\.com" }},
    {{ header = "Subject", regex = "(?i)urgent" }},
]
[rule.actions]
flag = true

# Move to folder
[[rule]]
name = "move alerts to subfolder"
[rule.match]
header = "Subject"
regex = "\\[ALERT\\]"
[rule.actions]
move_to = "INBOX/Alerts"

# Continue processing allows subsequent rules to also match
[[rule]]
name = "tag and continue"
continue_processing = true
[rule.match]
header = "To"
regex = "dev-team@"
[rule.actions]
flag = true

# Not condition
[[rule]]
name = "mark non-boss read"
[rule.match]
not = {{ header = "From", regex = "boss@" }}
[rule.actions]
mark_read = true

# Built-in spam classifier verdict (see the [spam] config section)
[[rule]]
name = "file spam"
[rule.match]
header = "X-Tmc-Spam-Verdict"
regex = "spam"
[rule.actions]
move_to = "junk"
```

Available match headers: From, To, Cc, Reply-To, Subject, Message-ID, plus any custom header (e.g. X-Spam-Score, X-Mailing-List).

The built-in Bayesian classifier scores new INBOX messages and injects two synthetic headers you can match on:
- X-Tmc-Spam-Verdict: "spam", "ham", or "unsure" (threshold already applied)
- X-Tmc-Spam-Score: the raw 0.0-1.0 score
Train it from the message view with `J` (mark spam) and `H` (mark not-spam); until trained past `[spam] min_training` per class, the verdict is always "unsure" so no rule fires.

Available actions:
- mark_read = true
- mark_unread = true
- flag = true
- unflag = true
- move_to = "MailboxName"  (supports name, role, or path like "INBOX/Sub")
- delete = true  (moves to Trash)

Conditions support: header/regex, all = [...], any = [...], not = {{...}}

Pattern dialect. Every `regex` is a POSIX Extended Regular Expression over bytes, with GNU's `\w \W \b \B` and an optional LEADING `(?i)` for case folding. Also accepted: `\d \D \s \S` as the ASCII classes, `(?:...)` as a plain group, `[[:word:]]` and `[[:ascii:]]`, and `\. \[ \] \( \) \| \+ \? \* \{{ \}} \^ \$ \\ \/ \-` as the literal character (`\t \n \r` as the byte).
Refused, each naming its byte offset: `\uXXXX`, `\x..`, `\p{{...}}`/`\P{{...}}` and any other unknown escape; a backreference; lookaround (`(?=` `(?!` `(?<=` `(?<!`); a named group; a comment group; an inline flag group that is not the leading `(?i)`; a non-greedy quantifier (`*?` `+?` `??` `{{n,m}}?`); `&&` inside a bracket expression; a negated shorthand inside one (`[\S]`); a non-ASCII character inside one; an unbalanced paren or bracket; and a pattern over 4 KiB. A refused pattern is a rules-file error naming the pattern and the reason.
Matching is POSIX leftmost-longest, so `x|xy` matches `xy`. `.` is one byte and matches a newline. `(?i)`, `\w`, `\b` and the named classes are ASCII, so `(?i)e` does not match an accented E. A pattern too expensive to decide counts as no match, so a rule that cannot be evaluated does not fire.

By default, only the first matching rule applies per email. Set `continue_processing = true` to allow subsequent rules to also match.
Set `skip_if_to_me = true` to skip a rule when `mail.my_email_regex` matches To or Cc.

Please ask me what kinds of emails I receive and how I want them organized, then generate a rules file.
"#,
                rules_path.display()
            );
        }
        _ => {
            eprintln!(
                "Unknown prompt topic '{}'. Available topics: config, rules",
                topic
            );
            std::process::exit(1);
        }
    }
}

fn print_help_config() {
    let config_path = default_config_path();
    println!("Default config file: {}", config_path.display());
    println!();
    println!("Available options:");
    println!();
    println!("[ui]");
    println!(
        "  editor = \"nvim\"              # Editor for composing (fallback: $EDITOR, then vi)"
    );
    println!("  browser = \"firefox\"           # Browser for opening URLs (fallback: $BROWSER, xdg-open)");
    println!("  page_size = 500              # Emails per page (default: 500)");
    println!(
        "  scrolloff = 1               # Keep this many context lines while scrolling (default: 1)"
    );
    println!("  mouse = true                 # Enable mouse support (default: true)");
    println!("  sync_interval_secs = 60      # Background sync interval in seconds (default: 60, 0 = off)");
    println!();
    println!("[mail]");
    println!("  archive_folder = \"archive\"   # Target folder for 'a' archive action (default: \"archive\")");
    println!("  deleted_folder = \"trash\"     # Target folder for 'd' delete action (default: \"trash\")");
    println!("  archive_mailbox_id = \"id\"    # Override archive folder by JMAP mailbox ID");
    println!("  deleted_mailbox_id = \"id\"    # Override deleted folder by JMAP mailbox ID");
    println!("  reply_from = \"Name <email>\"  # Override From header for replies/compose/forward");
    println!("  rules_mailbox_regex = \"^INBOX$\"  # Run rules only on matching mailbox names (default: \"^INBOX$\")");
    println!("  my_email_regex = \"^$\"        # Your email addresses for skip_if_to_me rule option (default: \"^$\")");
    println!();
    println!("[spam]                           # Built-in Bayesian spam classifier (scores new INBOX mail)");
    println!("  enabled = true               # Score new INBOX messages (default: true)");
    println!("  threshold = 0.9              # Score >= this is verdict \"spam\" (default: 0.9)");
    println!("  ham_threshold = 0.2          # Score <= this is verdict \"ham\"; between is \"unsure\" (default: 0.2)");
    println!("  min_training = 20            # Min trained messages per class before verdicts go live (default: 20)");
    println!("  # Train with J (spam) / H (not-spam) in the message view; act on the");
    println!("  # X-Tmc-Spam-Verdict header from rules.toml (see: td-mail --prompt=rules).");
    println!();
    println!("[account.NAME]                   # At least one account required");
    println!(
        "  well_known_url = \"https://.../.well-known/jmap\"  # JMAP discovery URL (required)"
    );
    println!("  username = \"user@example.com\"                    # Email address (required)");
    println!("  secret = \"portal\"                                # The credential portal, mail/NAME (this or password_command)");
    println!("  password_command = \"pass show email/example\"     # Shell command printing the password (or secret)");
    println!();
    println!("[retention.NAME]                 # Optional folder retention policies");
    println!("  folder = \"Archive\"            # Mailbox name to apply retention (required)");
    println!("  days = 365                   # Expire mail older than this many days (required)");
    println!();
    println!("[theme]                          # Optional color customization (#RRGGBB hex)");
    println!("  bg = \"#002b36\"               # Background color");
    println!("  fg = \"#839496\"               # Foreground color");
    println!("  bold_fg = \"#93a1a1\"          # Bold text color");
    println!("  selection_bg = \"#073642\"     # Selection background");
    println!("  selection_fg = \"#eee8d5\"     # Selection foreground");
    println!("  status_bg = \"#586e75\"        # Status bar background");
    println!("  status_fg = \"#eee8d5\"        # Status bar foreground");
    println!("  header_fg = \"#268bd2\"        # Header text color");
    println!();
    println!(
        "Legacy: [jmap] section with well_known_url, username, password_command is also supported."
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!("Usage: td-mail [OPTIONS]");
        eprintln!();
        eprintln!("Options:");
        eprintln!("  --config=PATH    Use config file at PATH instead of default");
        eprintln!("  --rules=PATH     Use rules file at PATH instead of default");
        eprintln!("  --clear-cache    Delete all local email cache files");
        eprintln!("  --clear-log      Truncate the log file at startup");
        eprintln!("  --log            View the log file in $PAGER");
        eprintln!("  --offline        Browse cached mail without network access");
        eprintln!("  --print-rules    Parse and print rules.toml");
        eprintln!("  --prompt=TOPIC   Print an AI-friendly prompt (config, rules)");
        eprintln!("  --cli            Run in JSON-over-stdin/stdout CLI mode");
        eprintln!("  --help-cli       Print CLI mode protocol documentation");
        eprintln!("  --help-config    Print default config path and all options");
        eprintln!("  --help           Show this help");
        std::process::exit(0);
    }

    if args.iter().any(|a| a == "--clear-cache") {
        cache::Cache::clear_all_accounts();
        eprintln!("Cache cleared.");
    }

    if args.iter().any(|a| a == "--clear-log") {
        if let Err(e) = log::clear() {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }

    if args.iter().any(|a| a == "--log") {
        show_log();
        std::process::exit(0);
    }

    if args.iter().any(|a| a == "--print-rules") {
        print_rules();
        std::process::exit(0);
    }

    if let Some(prompt_arg) = args.iter().find(|a| a.starts_with("--prompt=")) {
        let topic = &prompt_arg["--prompt=".len()..];
        print_prompt(topic);
        std::process::exit(0);
    }

    if args.iter().any(|a| a == "--prompt") {
        eprintln!("Usage: --prompt=TOPIC (available topics: config, rules)");
        std::process::exit(1);
    }

    if args.iter().any(|a| a == "--help-cli") {
        cli::print_help_cli();
        std::process::exit(0);
    }

    if args.iter().any(|a| a == "--help-config") {
        print_help_config();
        std::process::exit(0);
    }

    log::init();

    let config_path = args
        .iter()
        .find(|a| a.starts_with("--config="))
        .map(|a| PathBuf::from(&a["--config=".len()..]))
        .unwrap_or_else(default_config_path);

    let config = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error loading config from {}: {}", config_path.display(), e);
            eprintln!("Create a config file with:");
            eprintln!();
            eprintln!("  [account.personal]");
            eprintln!("  well_known_url = \"https://your-server/.well-known/jmap\"");
            eprintln!("  username = \"you@example.com\"");
            eprintln!("  secret = \"portal\"");
            std::process::exit(1);
        }
    };

    // Load rules (optional — missing file = no rules)
    let rules_path = args
        .iter()
        .find_map(|a| a.strip_prefix("--rules="))
        .map(PathBuf::from)
        .unwrap_or_else(|| match config_path.parent() {
            Some(dir) => dir.join("rules.toml"),
            // A bare relative config name has no parent; its rules sit beside
            // it in the working directory.
            None => PathBuf::from("rules.toml"),
        });
    let (compiled_rules, custom_headers) = if rules_path.exists() {
        match rules::load_rules(&rules_path) {
            Ok(rules) => {
                let headers = rules::extract_custom_headers(&rules);
                eprintln!(
                    "Loaded {} filtering rule(s){}",
                    rules.len(),
                    if headers.is_empty() {
                        String::new()
                    } else {
                        format!(" ({} custom header(s))", headers.len())
                    }
                );
                (rules, headers)
            }
            Err(e) => {
                eprintln!(
                    "Warning: failed to load rules from {}: {}",
                    rules_path.display(),
                    e
                );
                (Vec::new(), Vec::new())
            }
        }
    } else {
        (Vec::new(), Vec::new())
    };

    let offline = args.iter().any(|a| a == "--offline");

    if args.iter().any(|a| a == "--cli") {
        let archive_folder = config.mail.archive_folder.clone();
        let deleted_folder = config.mail.deleted_folder.clone();
        let archive_mailbox_id = config.mail.archive_mailbox_id.clone();
        let deleted_mailbox_id = config.mail.deleted_mailbox_id.clone();
        let rules_mailbox_regex = config.mail.rules_mailbox_regex.clone();
        let my_email_regex = config.mail.my_email_regex.clone();
        cli::run_cli(
            config,
            compiled_rules,
            custom_headers,
            rules_mailbox_regex,
            my_email_regex,
            archive_folder,
            deleted_folder,
            archive_mailbox_id,
            deleted_mailbox_id,
            offline,
        );
        std::process::exit(0);
    }

    // `Config::parse` refuses a configuration with no account, so this holds;
    // saying it here is cheaper than a proof in a comment.
    let Some(first_account) = config.accounts.first() else {
        eprintln!("No accounts configured in {}", config_path.display());
        std::process::exit(1);
    };

    let client = if offline {
        eprintln!("Offline mode ({})", first_account.name);
        None
    } else {
        // Connect to the first account
        eprint!(
            "Connecting to {} ({})...",
            first_account.name, first_account.well_known_url
        );
        io::stderr().flush().ok();

        match connect_account(first_account) {
            Ok(client) => {
                eprintln!(" OK");
                Some(client)
            }
            Err(e) => {
                // A server that is down, a network that is not up yet, or a
                // placeholder account: start from the cache instead of
                // exiting, so the window stays open and switching to the
                // account again retries the connection.
                eprintln!(" FAILED");
                eprintln!("{}", e);
                eprintln!("Starting offline; select the account again to reconnect.");
                log_error!("[Startup] connect to {} failed: {}", first_account.name, e);
                None
            }
        }
    };

    let first_account_name = first_account.name.clone();

    // Enter TUI
    let outcome = tui::run(
        client,
        config.accounts,
        0,
        first_account_name,
        config.ui.page_size,
        config.ui.scrolloff,
        config.ui.editor,
        config.ui.browser,
        config.ui.mouse,
        config.ui.sync_interval_secs,
        config.mail.archive_folder,
        config.mail.deleted_folder,
        config.mail.reply_from,
        config.mail.rules_mailbox_regex,
        config.mail.my_email_regex,
        config.mail.retention_policies,
        compiled_rules,
        custom_headers,
        config.theme,
        config.spam,
        offline,
    );

    // The raw-mode guard restores the terminal from a `Drop`, which has nowhere
    // to report from. This is the exit, and the operator staring at a terminal
    // with no echo is the only one who can act on it.
    if let Some(why) = term_sys::take_restore_failure() {
        eprintln!("td-mail: {}", why);
        eprintln!("td-mail: run `stty sane` to put the terminal back.");
        log_error!("[Exit] {}", why);
    }

    if let Err(e) = outcome {
        eprintln!("TUI error: {}", e);
        std::process::exit(1);
    }
}
