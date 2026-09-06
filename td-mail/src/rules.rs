use crate::jmap::client::JmapClient;
use crate::jmap::types::{Email, EmailAddress, Mailbox};
use crate::regex::UserRegex;
use crate::toml::{self, Error as TomlError, Toml};
use std::collections::{HashMap, HashSet};
use std::path::Path;

// --- TOML mapping types ---

#[derive(Debug)]
pub struct RulesConfig {
    pub rule: Vec<RuleDef>,
}

impl RulesConfig {
    /// Read a rules file. Unknown keys are ignored, as serde's default was;
    /// a missing required key reads with serde's own wording.
    pub fn parse(contents: &str) -> Result<Self, String> {
        let document = toml::parse(contents).map_err(|e| e.to_string())?;
        RulesConfig::from_toml(&document).map_err(|e| e.to_string())
    }

    fn from_toml(root: &Toml) -> Result<Self, TomlError> {
        let entries = match root.optional_arr("rule")? {
            Some(entries) => entries,
            None => return Ok(RulesConfig { rule: Vec::new() }),
        };
        let mut rule = Vec::with_capacity(entries.len());
        for entry in entries {
            rule.push(RuleDef::from_toml(entry)?);
        }
        Ok(RulesConfig { rule })
    }
}

#[derive(Debug)]
pub struct RuleDef {
    pub name: String,
    pub continue_processing: Option<bool>,
    pub skip_if_to_me: Option<bool>,
    pub match_condition: ConditionDef,
    pub actions: ActionsDef,
    pub triage: Option<TriageHintDef>,
}

impl RuleDef {
    fn from_toml(table: &Toml) -> Result<Self, TomlError> {
        let triage = match table.optional_table("triage")? {
            Some(t) => Some(TriageHintDef::from_toml(t)?),
            None => None,
        };
        Ok(RuleDef {
            name: table.require_str("name")?.to_string(),
            continue_processing: table.optional_bool("continue_processing")?,
            skip_if_to_me: table.optional_bool("skip_if_to_me")?,
            match_condition: ConditionDef::from_toml(table.require_table("match")?)?,
            actions: ActionsDef::from_toml(table.require_table("actions")?)?,
            triage,
        })
    }
}

#[derive(Debug)]
pub struct TriageHintDef {
    pub action: TriageHintActionDef,
    pub confidence: Option<f32>,
}

impl TriageHintDef {
    fn from_toml(table: &Toml) -> Result<Self, TomlError> {
        Ok(TriageHintDef {
            action: TriageHintActionDef::from_toml(table.require_str("action")?)?,
            confidence: table.optional_float("confidence")?.map(|v| v as f32),
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub enum TriageHintActionDef {
    Archive,
    Trash,
    Keep,
}

impl TriageHintActionDef {
    fn from_toml(name: &str) -> Result<Self, TomlError> {
        match name {
            "archive" => Ok(TriageHintActionDef::Archive),
            "trash" => Ok(TriageHintActionDef::Trash),
            "keep" => Ok(TriageHintActionDef::Keep),
            other => Err(TomlError::new(format!(
                "unknown variant `{}`, expected one of `archive`, `trash`, `keep`",
                other
            ))),
        }
    }
}

/// `[rule.match]`. Serde read this as an untagged enum: each shape is tried
/// in turn and the first that fits wins, so the discriminator is which keys
/// the table carries.
#[derive(Debug)]
pub enum ConditionDef {
    Header { header: String, regex: String },
    All { all: Vec<ConditionDef> },
    Any { any: Vec<ConditionDef> },
    Not { not: Box<ConditionDef> },
}

impl ConditionDef {
    fn from_toml(table: &Toml) -> Result<Self, TomlError> {
        if let (Some(header), Some(regex)) =
            (table.optional_str("header")?, table.optional_str("regex")?)
        {
            return Ok(ConditionDef::Header {
                header: header.to_string(),
                regex: regex.to_string(),
            });
        }
        if let Some(items) = table.optional_arr("all")? {
            return Ok(ConditionDef::All {
                all: ConditionDef::list(items)?,
            });
        }
        if let Some(items) = table.optional_arr("any")? {
            return Ok(ConditionDef::Any {
                any: ConditionDef::list(items)?,
            });
        }
        if let Some(inner) = table.optional_table("not")? {
            return Ok(ConditionDef::Not {
                not: Box::new(ConditionDef::from_toml(inner)?),
            });
        }
        Err(TomlError::new(
            "data did not match any variant of untagged enum ConditionDef",
        ))
    }

    fn list(items: &[Toml]) -> Result<Vec<ConditionDef>, TomlError> {
        items.iter().map(ConditionDef::from_toml).collect()
    }
}

#[derive(Debug)]
pub struct ActionsDef {
    pub mark_read: Option<bool>,
    pub mark_unread: Option<bool>,
    pub flag: Option<bool>,
    pub unflag: Option<bool>,
    pub move_to: Option<String>,
    pub delete: Option<bool>,
}

impl ActionsDef {
    fn from_toml(table: &Toml) -> Result<Self, TomlError> {
        Ok(ActionsDef {
            mark_read: table.optional_bool("mark_read")?,
            mark_unread: table.optional_bool("mark_unread")?,
            flag: table.optional_bool("flag")?,
            unflag: table.optional_bool("unflag")?,
            move_to: table.optional_str("move_to")?.map(str::to_string),
            delete: table.optional_bool("delete")?,
        })
    }
}

// --- Compiled types ---

#[derive(Debug)]
pub struct CompiledRule {
    pub name: String,
    pub continue_processing: bool,
    pub skip_if_to_me: bool,
    pub actions: Vec<Action>,
    pub condition: CompiledCondition,
    pub triage_action: Option<TriageHintActionDef>,
    pub triage_confidence: Option<f32>,
}

#[derive(Debug, Clone)]
pub enum Action {
    MarkRead,
    MarkUnread,
    Flag,
    Unflag,
    Move { target: String },
    Delete,
}

#[derive(Debug)]
pub enum CompiledCondition {
    Header { header: String, regex: UserRegex },
    All(Vec<CompiledCondition>),
    Any(Vec<CompiledCondition>),
    Not(Box<CompiledCondition>),
}

// --- Loading and compilation ---

pub fn load_rules(path: &Path) -> Result<Vec<CompiledRule>, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("Failed to read rules file: {}", e))?;
    let config =
        RulesConfig::parse(&content).map_err(|e| format!("Failed to parse rules TOML: {}", e))?;

    config
        .rule
        .into_iter()
        .map(compile_rule)
        .collect::<Result<Vec<_>, _>>()
}

pub fn format_rules_for_display(rules: &[CompiledRule]) -> String {
    if rules.is_empty() {
        return "No rules defined.".to_string();
    }

    let mut out = String::new();
    for (idx, rule) in rules.iter().enumerate() {
        out.push_str(&format!("Rule {}: {}\n", idx + 1, rule.name));
        out.push_str(&format!(
            "  Continue processing: {}\n",
            if rule.continue_processing {
                "yes"
            } else {
                "no"
            }
        ));
        out.push_str(&format!(
            "  Skip if not to me: {}\n",
            if rule.skip_if_to_me { "yes" } else { "no" }
        ));
        out.push_str(&format!(
            "  Match: {}\n",
            format_condition_for_display(&rule.condition)
        ));
        out.push_str(&format!(
            "  Actions: {}\n",
            format_actions_for_display(&rule.actions)
        ));
    }
    out
}

fn compile_rule(def: RuleDef) -> Result<CompiledRule, String> {
    let actions = compile_actions(&def.actions);
    let condition = compile_condition(def.match_condition)
        .map_err(|e| format!("Rule '{}': {}", def.name, e))?;

    Ok(CompiledRule {
        name: def.name,
        continue_processing: def.continue_processing.unwrap_or(false),
        skip_if_to_me: def.skip_if_to_me.unwrap_or(false),
        actions,
        condition,
        triage_action: def.triage.as_ref().map(|t| match t.action {
            TriageHintActionDef::Archive => TriageHintActionDef::Archive,
            TriageHintActionDef::Trash => TriageHintActionDef::Trash,
            TriageHintActionDef::Keep => TriageHintActionDef::Keep,
        }),
        triage_confidence: def.triage.and_then(|t| t.confidence),
    })
}

fn format_condition_for_display(condition: &CompiledCondition) -> String {
    match condition {
        CompiledCondition::Header { header, regex } => {
            format!("{} =~ /{}/", header, regex.as_str())
        }
        CompiledCondition::All(conditions) => {
            let parts: Vec<String> = conditions
                .iter()
                .map(format_condition_for_display)
                .collect();
            format!("all({})", parts.join(", "))
        }
        CompiledCondition::Any(conditions) => {
            let parts: Vec<String> = conditions
                .iter()
                .map(format_condition_for_display)
                .collect();
            format!("any({})", parts.join(", "))
        }
        CompiledCondition::Not(inner) => {
            format!("not({})", format_condition_for_display(inner))
        }
    }
}

fn format_actions_for_display(actions: &[Action]) -> String {
    if actions.is_empty() {
        return "(none)".to_string();
    }

    let parts: Vec<String> = actions
        .iter()
        .map(|a| match a {
            Action::MarkRead => "mark_read".to_string(),
            Action::MarkUnread => "mark_unread".to_string(),
            Action::Flag => "flag".to_string(),
            Action::Unflag => "unflag".to_string(),
            Action::Move { target } => format!("move_to={}", target),
            Action::Delete => "delete".to_string(),
        })
        .collect();
    parts.join(", ")
}

fn compile_actions(def: &ActionsDef) -> Vec<Action> {
    let mut actions = Vec::new();
    if def.mark_read == Some(true) {
        actions.push(Action::MarkRead);
    }
    if def.mark_unread == Some(true) {
        actions.push(Action::MarkUnread);
    }
    if def.flag == Some(true) {
        actions.push(Action::Flag);
    }
    if def.unflag == Some(true) {
        actions.push(Action::Unflag);
    }
    if let Some(ref target) = def.move_to {
        actions.push(Action::Move {
            target: target.clone(),
        });
    }
    if def.delete == Some(true) {
        actions.push(Action::Delete);
    }
    actions
}

fn compile_condition(def: ConditionDef) -> Result<CompiledCondition, String> {
    match def {
        ConditionDef::Header { header, regex } => {
            let compiled = UserRegex::compile(&regex)
                .map_err(|e| format!("Invalid regex '{}': {}", regex, e))?;
            Ok(CompiledCondition::Header {
                header,
                regex: compiled,
            })
        }
        ConditionDef::All { all } => {
            let conditions = all
                .into_iter()
                .map(compile_condition)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CompiledCondition::All(conditions))
        }
        ConditionDef::Any { any } => {
            let conditions = any
                .into_iter()
                .map(compile_condition)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CompiledCondition::Any(conditions))
        }
        ConditionDef::Not { not } => {
            let inner = compile_condition(*not)?;
            Ok(CompiledCondition::Not(Box::new(inner)))
        }
    }
}

// --- Header extraction for JMAP ---

/// Collect custom (non-standard) header names from all rules and return
/// them as JMAP property strings like `header:X-Custom:asText`.
pub fn extract_custom_headers(rules: &[CompiledRule]) -> Vec<String> {
    let mut headers = std::collections::HashSet::new();
    for rule in rules {
        collect_headers_from_condition(&rule.condition, &mut headers);
    }
    headers
        .into_iter()
        .filter(|h| !is_standard_header(h))
        .map(|h| format!("header:{}:asText", h))
        .collect()
}

fn collect_headers_from_condition(
    condition: &CompiledCondition,
    headers: &mut std::collections::HashSet<String>,
) {
    match condition {
        CompiledCondition::Header { header, .. } => {
            headers.insert(header.clone());
        }
        CompiledCondition::All(conditions) | CompiledCondition::Any(conditions) => {
            for c in conditions {
                collect_headers_from_condition(c, headers);
            }
        }
        CompiledCondition::Not(inner) => {
            collect_headers_from_condition(inner, headers);
        }
    }
}

fn is_standard_header(header: &str) -> bool {
    matches!(
        header.to_lowercase().as_str(),
        "from" | "to" | "cc" | "reply-to" | "subject" | "message-id"
    )
}

// --- Condition evaluation ---

pub fn evaluate_condition(condition: &CompiledCondition, email: &Email) -> bool {
    match condition {
        CompiledCondition::Header { header, regex } => match resolve_header_value(header, email) {
            Some(value) => regex.is_match(&value),
            None => false,
        },
        CompiledCondition::All(conditions) => {
            conditions.iter().all(|c| evaluate_condition(c, email))
        }
        CompiledCondition::Any(conditions) => {
            conditions.iter().any(|c| evaluate_condition(c, email))
        }
        CompiledCondition::Not(inner) => !evaluate_condition(inner, email),
    }
}

fn format_addresses(addrs: &Option<Vec<EmailAddress>>) -> Option<String> {
    addrs.as_ref().map(|list| {
        list.iter()
            .map(|a| format!("{}", a))
            .collect::<Vec<_>>()
            .join(", ")
    })
}

fn resolve_header_value(header: &str, email: &Email) -> Option<String> {
    match header.to_lowercase().as_str() {
        "from" => format_addresses(&email.from),
        "to" => format_addresses(&email.to),
        "cc" => format_addresses(&email.cc),
        "reply-to" => format_addresses(&email.reply_to),
        "subject" => email.subject.clone(),
        "message-id" => email.message_id.as_ref().map(|ids| ids.join(" ")),
        _ => {
            // Look up in extra properties (custom JMAP headers)
            let key = format!("header:{}:asText", header);
            email
                .extra
                .get(&key)
                .and_then(|v| v.as_str().map(|s| s.to_string()))
        }
    }
}

// --- Rule application ---

pub struct RuleApplication {
    pub email_id: String,
    pub rule_name: String,
    pub actions: Vec<Action>,
}

/// Evaluate all rules against fetched emails, returning the list of actions to execute.
/// Actions are idempotent: skip no-ops (e.g. mark_read on already-read emails).
pub fn apply_rules(
    rules: &[CompiledRule],
    emails: &[Email],
    mailboxes: &[Mailbox],
    my_email_regex: &UserRegex,
) -> Vec<RuleApplication> {
    let mut applications = Vec::new();
    let mut total_actions = 0usize;

    log_debug!(
        "[Rules] Evaluating {} rule(s) against {} email(s)",
        rules.len(),
        emails.len()
    );

    for email in emails {
        let mut matched_rule_names = Vec::new();
        let mut queued_rule_summaries = Vec::new();
        for rule in rules {
            if rule.skip_if_to_me && is_email_to_me(email, my_email_regex) {
                continue;
            }
            let matched = evaluate_condition(&rule.condition, email);

            if matched {
                matched_rule_names.push(rule.name.clone());
                let filtered_actions = filter_noop_actions(&rule.actions, email, mailboxes);

                if !filtered_actions.is_empty() {
                    total_actions += filtered_actions.len();
                    let action_names = filtered_actions
                        .iter()
                        .map(format_action_name)
                        .collect::<Vec<_>>()
                        .join(", ");
                    queued_rule_summaries.push(format!("{} -> {}", rule.name, action_names));
                    applications.push(RuleApplication {
                        email_id: email.id.clone(),
                        rule_name: rule.name.clone(),
                        actions: filtered_actions,
                    });
                }

                if !rule.continue_processing {
                    break;
                }
            }
        }

        let subject = email.subject.as_deref().unwrap_or("(none)");
        let to_line = format_addresses(&email.to).unwrap_or_else(|| "(none)".to_string());
        let matched = if matched_rule_names.is_empty() {
            "(none)".to_string()
        } else {
            matched_rule_names.join(", ")
        };
        let queued = if queued_rule_summaries.is_empty() {
            "(none)".to_string()
        } else {
            queued_rule_summaries.join("; ")
        };

        log_info!(
            "[Rules] Email {} subject='{}' to='{}' matched_rules=[{}] queued_actions=[{}]",
            email.id,
            subject,
            to_line,
            matched,
            queued
        );
    }

    log_info!(
        "[Rules] Evaluation complete: {} application(s), {} action(s)",
        applications.len(),
        total_actions
    );
    applications
}

fn is_email_to_me(email: &Email, my_email_regex: &UserRegex) -> bool {
    let mut combined = String::new();
    if let Some(to) = format_addresses(&email.to) {
        combined.push_str(&to);
    }
    if let Some(cc) = format_addresses(&email.cc) {
        if !combined.is_empty() {
            combined.push_str(", ");
        }
        combined.push_str(&cc);
    }
    if combined.is_empty() {
        return false;
    }
    my_email_regex.is_match(&combined)
}

fn filter_noop_actions(actions: &[Action], email: &Email, mailboxes: &[Mailbox]) -> Vec<Action> {
    let mut filtered = Vec::new();
    for action in actions {
        let keep = match action {
            Action::MarkRead => {
                let keep = !email.keywords.contains_key("$seen");
                if !keep {
                    log_debug!("[Rules] Email {} skip mark_read: already read", email.id);
                }
                keep
            }
            Action::MarkUnread => {
                let keep = email.keywords.contains_key("$seen");
                if !keep {
                    log_debug!(
                        "[Rules] Email {} skip mark_unread: already unread",
                        email.id
                    );
                }
                keep
            }
            Action::Flag => {
                let keep = !email.keywords.contains_key("$flagged");
                if !keep {
                    log_debug!("[Rules] Email {} skip flag: already flagged", email.id);
                }
                keep
            }
            Action::Unflag => {
                let keep = email.keywords.contains_key("$flagged");
                if !keep {
                    log_debug!("[Rules] Email {} skip unflag: not flagged", email.id);
                }
                keep
            }
            Action::Move { target } => {
                if let Some(target_id) = resolve_mailbox_id(target, mailboxes) {
                    let keep = !email.mailbox_ids.contains_key(&target_id);
                    if !keep {
                        log_debug!(
                            "[Rules] Email {} skip move_to='{}': already in target mailbox",
                            email.id,
                            target
                        );
                    }
                    keep
                } else {
                    log_warn!(
                        "[Rules] Email {} skip move_to='{}': target mailbox cannot be resolved",
                        email.id,
                        target
                    );
                    false
                }
            }
            Action::Delete => true,
        };

        if keep {
            filtered.push(action.clone());
        }
    }
    filtered
}

/// Resolve a mailbox name/path to a JMAP mailbox ID.
/// Supports simple names ("Archive") and paths ("INBOX/Alerts").
pub fn resolve_mailbox_id(name: &str, mailboxes: &[Mailbox]) -> Option<String> {
    // First try exact name match
    if let Some(mbox) = mailboxes.iter().find(|m| m.name == name) {
        log_debug!(
            "[Rules] Resolved mailbox '{}' by exact name -> {}",
            name,
            mbox.id
        );
        return Some(mbox.id.clone());
    }

    // Try role match (e.g. "trash", "archive", "junk")
    let lower = name.to_lowercase();
    if let Some(mbox) = mailboxes
        .iter()
        .find(|m| m.role.as_ref().map(|r| r.to_lowercase()) == Some(lower.clone()))
    {
        log_debug!("[Rules] Resolved mailbox '{}' by role -> {}", name, mbox.id);
        return Some(mbox.id.clone());
    }

    // Try path match: "Parent/Child"
    if name.contains('/') {
        let parts: Vec<&str> = name.split('/').collect();
        // `split` yields at least one part, so the leaf is the last of them;
        // the whole name is the same answer for a path with no separator.
        let leaf = parts.last().copied().unwrap_or(name);
        let mailbox_map: HashMap<String, &Mailbox> =
            mailboxes.iter().map(|m| (m.id.clone(), m)).collect();

        // Find leaf mailbox matching the last path component
        for mbox in mailboxes {
            if mbox.name == leaf {
                // Walk up the parent chain to verify full path
                let mut path = vec![mbox.name.as_str()];
                let mut current = mbox;
                while let Some(ref pid) = current.parent_id {
                    if let Some(parent) = mailbox_map.get(pid) {
                        path.push(parent.name.as_str());
                        current = parent;
                    } else {
                        break;
                    }
                }
                path.reverse();
                let full_path = path.join("/");
                if full_path == name {
                    log_debug!("[Rules] Resolved mailbox '{}' by path -> {}", name, mbox.id);
                    return Some(mbox.id.clone());
                }
            }
        }
    }

    log_warn!("[Rules] Failed to resolve mailbox '{}'", name);
    None
}

/// Execute rule applications against the JMAP server.
/// Returns the set of email IDs that were moved or deleted (removed from current mailbox).
#[allow(dead_code)]
pub fn execute_rule_actions(
    applications: &[RuleApplication],
    mailboxes: &[Mailbox],
    client: &JmapClient,
) -> HashSet<String> {
    let mut removed_ids = HashSet::new();
    let action_count = applications.iter().map(|a| a.actions.len()).sum::<usize>();
    log_info!(
        "[Rules] Executing {} action(s) from {} application(s)",
        action_count,
        applications.len()
    );

    for app in applications {
        log_debug!(
            "[Rules] Executing application for email {} rule '{}' ({} action(s))",
            app.email_id,
            app.rule_name,
            app.actions.len()
        );
        for action in &app.actions {
            log_debug!(
                "[Rules] Attempting action {} on email {} (rule: {})",
                format_action_name(action),
                app.email_id,
                app.rule_name
            );
            let result = match action {
                Action::MarkRead => client.mark_email_read(&app.email_id),
                Action::MarkUnread => client.mark_email_unread(&app.email_id),
                Action::Flag => client.set_email_flagged(&app.email_id, true),
                Action::Unflag => client.set_email_flagged(&app.email_id, false),
                Action::Move { target } => {
                    if let Some(target_id) = resolve_mailbox_id(target, mailboxes) {
                        client.move_email(&app.email_id, &target_id)
                    } else {
                        log_warn!(
                            "[Rules] Cannot resolve mailbox '{}' for rule '{}'",
                            target,
                            app.rule_name
                        );
                        continue;
                    }
                }
                Action::Delete => {
                    // Move to Trash
                    if let Some(trash_id) = mailboxes
                        .iter()
                        .find(|m| m.role.as_deref() == Some("trash"))
                        .map(|m| m.id.clone())
                    {
                        client.move_email(&app.email_id, &trash_id)
                    } else {
                        log_warn!(
                            "[Rules] No Trash mailbox found for delete action in rule '{}'",
                            app.rule_name
                        );
                        continue;
                    }
                }
            };

            match result {
                Ok(()) => {
                    log_info!(
                        "[Rules] Applied {:?} to email {} (rule: {})",
                        action,
                        app.email_id,
                        app.rule_name
                    );
                    if matches!(action, Action::Move { .. } | Action::Delete) {
                        removed_ids.insert(app.email_id.clone());
                    }
                }
                Err(e) => {
                    log_warn!(
                        "[Rules] Failed to apply {:?} to email {} (rule: {}): {}",
                        action,
                        app.email_id,
                        app.rule_name,
                        e
                    );
                }
            }
        }
    }
    removed_ids
}

fn format_action_name(action: &Action) -> String {
    match action {
        Action::MarkRead => "mark_read".to_string(),
        Action::MarkUnread => "mark_unread".to_string(),
        Action::Flag => "flag".to_string(),
        Action::Unflag => "unflag".to_string(),
        Action::Move { target } => format!("move_to={}", target),
        Action::Delete => "delete".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_email(id: &str) -> Email {
        Email {
            id: id.to_string(),
            thread_id: None,
            from: Some(vec![EmailAddress {
                name: Some("Alice".to_string()),
                email: Some("alice@example.com".to_string()),
            }]),
            to: Some(vec![EmailAddress {
                name: Some("Bob".to_string()),
                email: Some("bob@example.com".to_string()),
            }]),
            cc: None,
            reply_to: None,
            subject: Some("Test Subject".to_string()),
            received_at: None,
            sent_at: None,
            preview: None,
            text_body: None,
            html_body: None,
            body_values: HashMap::new(),
            keywords: HashMap::new(),
            mailbox_ids: HashMap::new(),
            message_id: None,
            references: None,
            attachments: None,
            extra: HashMap::new(),
        }
    }

    #[test]
    fn test_parse_simple_rule() {
        let toml_str = r#"
[[rule]]
name = "mark newsletters read"
[rule.match]
header = "From"
regex = "newsletter@"
[rule.actions]
mark_read = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        assert_eq!(config.rule.len(), 1);
        assert_eq!(config.rule[0].name, "mark newsletters read");
    }

    #[test]
    fn test_parse_all_condition() {
        let toml_str = r#"
[[rule]]
name = "complex rule"
[rule.match]
all = [
    { header = "From", regex = "alice@" },
    { header = "Subject", regex = "urgent" },
]
[rule.actions]
flag = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        assert_eq!(config.rule.len(), 1);
    }

    #[test]
    fn test_parse_any_condition() {
        let toml_str = r#"
[[rule]]
name = "any rule"
[rule.match]
any = [
    { header = "From", regex = "alice@" },
    { header = "From", regex = "bob@" },
]
[rule.actions]
mark_read = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        assert_eq!(config.rule.len(), 1);
    }

    #[test]
    fn test_parse_not_condition() {
        let toml_str = r#"
[[rule]]
name = "not rule"
[rule.match]
not = { header = "From", regex = "boss@" }
[rule.actions]
mark_read = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        assert_eq!(config.rule.len(), 1);
    }

    #[test]
    fn test_parse_move_action() {
        let toml_str = r#"
[[rule]]
name = "move alerts"
[rule.match]
header = "Subject"
regex = "\\[ALERT\\]"
[rule.actions]
move_to = "INBOX/Alerts"
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        assert_eq!(config.rule.len(), 1);
    }

    #[test]
    fn test_parse_continue_processing() {
        let toml_str = r#"
[[rule]]
name = "first rule"
continue_processing = true
[rule.match]
header = "From"
regex = "alice@"
[rule.actions]
flag = true

[[rule]]
name = "second rule"
[rule.match]
header = "Subject"
regex = "Test"
[rule.actions]
mark_read = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        assert_eq!(config.rule.len(), 2);
        assert_eq!(config.rule[0].continue_processing, Some(true));
        assert_eq!(config.rule[0].skip_if_to_me, None);
        assert_eq!(config.rule[1].continue_processing, None);
        assert_eq!(config.rule[1].skip_if_to_me, None);
    }

    #[test]
    fn test_compile_and_evaluate_header_match() {
        let toml_str = r#"
[[rule]]
name = "from alice"
[rule.match]
header = "From"
regex = "alice@"
[rule.actions]
mark_read = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        let rules: Vec<CompiledRule> = config
            .rule
            .into_iter()
            .map(compile_rule)
            .collect::<Result<_, _>>()
            .unwrap();

        let email = make_email("e1");
        assert!(evaluate_condition(&rules[0].condition, &email));
    }

    #[test]
    fn test_evaluate_no_match() {
        let toml_str = r#"
[[rule]]
name = "from bob"
[rule.match]
header = "From"
regex = "charlie@"
[rule.actions]
mark_read = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        let rules: Vec<CompiledRule> = config
            .rule
            .into_iter()
            .map(compile_rule)
            .collect::<Result<_, _>>()
            .unwrap();

        let email = make_email("e1");
        assert!(!evaluate_condition(&rules[0].condition, &email));
    }

    #[test]
    fn test_evaluate_subject_match() {
        let toml_str = r#"
[[rule]]
name = "subject test"
[rule.match]
header = "Subject"
regex = "(?i)test"
[rule.actions]
flag = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        let rules: Vec<CompiledRule> = config
            .rule
            .into_iter()
            .map(compile_rule)
            .collect::<Result<_, _>>()
            .unwrap();

        let email = make_email("e1");
        assert!(evaluate_condition(&rules[0].condition, &email));
    }

    #[test]
    fn test_evaluate_all_condition() {
        let toml_str = r#"
[[rule]]
name = "all cond"
[rule.match]
all = [
    { header = "From", regex = "alice@" },
    { header = "Subject", regex = "Test" },
]
[rule.actions]
mark_read = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        let rules: Vec<CompiledRule> = config
            .rule
            .into_iter()
            .map(compile_rule)
            .collect::<Result<_, _>>()
            .unwrap();

        let email = make_email("e1");
        assert!(evaluate_condition(&rules[0].condition, &email));
    }

    #[test]
    fn test_evaluate_any_condition() {
        let toml_str = r#"
[[rule]]
name = "any cond"
[rule.match]
any = [
    { header = "From", regex = "charlie@" },
    { header = "From", regex = "alice@" },
]
[rule.actions]
mark_read = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        let rules: Vec<CompiledRule> = config
            .rule
            .into_iter()
            .map(compile_rule)
            .collect::<Result<_, _>>()
            .unwrap();

        let email = make_email("e1");
        assert!(evaluate_condition(&rules[0].condition, &email));
    }

    #[test]
    fn test_evaluate_not_condition() {
        let toml_str = r#"
[[rule]]
name = "not cond"
[rule.match]
not = { header = "From", regex = "charlie@" }
[rule.actions]
mark_read = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        let rules: Vec<CompiledRule> = config
            .rule
            .into_iter()
            .map(compile_rule)
            .collect::<Result<_, _>>()
            .unwrap();

        let email = make_email("e1");
        assert!(evaluate_condition(&rules[0].condition, &email));
    }

    #[test]
    fn test_extract_custom_headers() {
        let toml_str = r#"
[[rule]]
name = "custom header"
[rule.match]
all = [
    { header = "From", regex = "test" },
    { header = "X-Spam-Score", regex = "5" },
]
[rule.actions]
mark_read = true

[[rule]]
name = "another"
[rule.match]
header = "X-Mailing-List"
regex = "dev"
[rule.actions]
flag = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        let rules: Vec<CompiledRule> = config
            .rule
            .into_iter()
            .map(compile_rule)
            .collect::<Result<_, _>>()
            .unwrap();

        let custom = extract_custom_headers(&rules);
        assert!(custom.contains(&"header:X-Spam-Score:asText".to_string()));
        assert!(custom.contains(&"header:X-Mailing-List:asText".to_string()));
        assert_eq!(custom.len(), 2);
    }

    #[test]
    fn test_no_custom_headers_for_standard() {
        let toml_str = r#"
[[rule]]
name = "standard only"
[rule.match]
header = "From"
regex = "test"
[rule.actions]
mark_read = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        let rules: Vec<CompiledRule> = config
            .rule
            .into_iter()
            .map(compile_rule)
            .collect::<Result<_, _>>()
            .unwrap();

        let custom = extract_custom_headers(&rules);
        assert!(custom.is_empty());
    }

    #[test]
    fn test_filter_noop_mark_read_already_seen() {
        let mut email = make_email("e1");
        email.keywords.insert("$seen".to_string(), true);

        let actions = vec![Action::MarkRead];
        let filtered = filter_noop_actions(&actions, &email, &[]);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_filter_noop_flag_already_flagged() {
        let mut email = make_email("e1");
        email.keywords.insert("$flagged".to_string(), true);

        let actions = vec![Action::Flag];
        let filtered = filter_noop_actions(&actions, &email, &[]);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_filter_noop_move_already_in_mailbox() {
        let mut email = make_email("e1");
        email.mailbox_ids.insert("mbox-1".to_string(), true);

        let mailboxes = vec![Mailbox {
            id: "mbox-1".to_string(),
            name: "Archive".to_string(),
            parent_id: None,
            role: Some("archive".to_string()),
            total_emails: 0,
            unread_emails: 0,
            sort_order: 0,
        }];

        let actions = vec![Action::Move {
            target: "Archive".to_string(),
        }];
        let filtered = filter_noop_actions(&actions, &email, &mailboxes);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_apply_rules_stops_after_first_match() {
        let toml_str = r#"
[[rule]]
name = "first"
[rule.match]
header = "From"
regex = "alice@"
[rule.actions]
mark_read = true

[[rule]]
name = "second"
[rule.match]
header = "From"
regex = "alice@"
[rule.actions]
flag = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        let rules: Vec<CompiledRule> = config
            .rule
            .into_iter()
            .map(compile_rule)
            .collect::<Result<_, _>>()
            .unwrap();

        let email = make_email("e1");
        let my_email_regex = UserRegex::compile(".*").unwrap();
        let apps = apply_rules(&rules, &[email], &[], &my_email_regex);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].rule_name, "first");
    }

    #[test]
    fn test_apply_rules_continue_processing() {
        let toml_str = r#"
[[rule]]
name = "first"
continue_processing = true
[rule.match]
header = "From"
regex = "alice@"
[rule.actions]
flag = true

[[rule]]
name = "second"
[rule.match]
header = "Subject"
regex = "Test"
[rule.actions]
mark_read = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        let rules: Vec<CompiledRule> = config
            .rule
            .into_iter()
            .map(compile_rule)
            .collect::<Result<_, _>>()
            .unwrap();

        let email = make_email("e1");
        let my_email_regex = UserRegex::compile(".*").unwrap();
        let apps = apply_rules(&rules, &[email], &[], &my_email_regex);
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].rule_name, "first");
        assert_eq!(apps[1].rule_name, "second");
    }

    #[test]
    fn test_skip_if_to_me_does_not_skip_when_to_or_cc_does_not_match() {
        let toml_str = r#"
[[rule]]
name = "match if addressed to me"
skip_if_to_me = true
[rule.match]
header = "From"
regex = "alice@"
[rule.actions]
flag = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        let rules: Vec<CompiledRule> = config
            .rule
            .into_iter()
            .map(compile_rule)
            .collect::<Result<_, _>>()
            .unwrap();

        let email = make_email("e1");
        let my_email_regex = UserRegex::compile("(?i)me@example\\.com").unwrap();
        let apps = apply_rules(&rules, &[email], &[], &my_email_regex);
        assert_eq!(apps.len(), 1);
    }

    #[test]
    fn test_skip_if_to_me_skips_when_to_or_cc_matches() {
        let toml_str = r#"
[[rule]]
name = "match if addressed to me"
skip_if_to_me = true
[rule.match]
header = "From"
regex = "alice@"
[rule.actions]
flag = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        let rules: Vec<CompiledRule> = config
            .rule
            .into_iter()
            .map(compile_rule)
            .collect::<Result<_, _>>()
            .unwrap();

        let mut email = make_email("e1");
        email.to = Some(vec![EmailAddress {
            name: Some("Timmy".to_string()),
            email: Some("me@example.com".to_string()),
        }]);
        let my_email_regex = UserRegex::compile("(?i)me@example\\.com").unwrap();
        let apps = apply_rules(&rules, &[email], &[], &my_email_regex);
        assert!(apps.is_empty());
    }

    #[test]
    fn test_skip_if_to_me_skips_when_cc_matches() {
        let toml_str = r#"
[[rule]]
name = "match if addressed to me"
skip_if_to_me = true
[rule.match]
header = "From"
regex = "alice@"
[rule.actions]
flag = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        let rules: Vec<CompiledRule> = config
            .rule
            .into_iter()
            .map(compile_rule)
            .collect::<Result<_, _>>()
            .unwrap();

        let mut email = make_email("e1");
        email.cc = Some(vec![EmailAddress {
            name: Some("Timmy".to_string()),
            email: Some("me@example.com".to_string()),
        }]);
        let my_email_regex = UserRegex::compile("(?i)me@example\\.com").unwrap();
        let apps = apply_rules(&rules, &[email], &[], &my_email_regex);
        assert!(apps.is_empty());
    }

    #[test]
    fn test_resolve_mailbox_by_name() {
        let mailboxes = vec![Mailbox {
            id: "mbox-1".to_string(),
            name: "Archive".to_string(),
            parent_id: None,
            role: Some("archive".to_string()),
            total_emails: 0,
            unread_emails: 0,
            sort_order: 0,
        }];
        assert_eq!(
            resolve_mailbox_id("Archive", &mailboxes),
            Some("mbox-1".to_string())
        );
    }

    #[test]
    fn test_resolve_mailbox_by_role() {
        let mailboxes = vec![Mailbox {
            id: "mbox-trash".to_string(),
            name: "Deleted Items".to_string(),
            parent_id: None,
            role: Some("trash".to_string()),
            total_emails: 0,
            unread_emails: 0,
            sort_order: 0,
        }];
        assert_eq!(
            resolve_mailbox_id("trash", &mailboxes),
            Some("mbox-trash".to_string())
        );
    }

    #[test]
    fn test_resolve_mailbox_by_path() {
        let mailboxes = vec![
            Mailbox {
                id: "mbox-inbox".to_string(),
                name: "INBOX".to_string(),
                parent_id: None,
                role: Some("inbox".to_string()),
                total_emails: 0,
                unread_emails: 0,
                sort_order: 0,
            },
            Mailbox {
                id: "mbox-alerts".to_string(),
                name: "Alerts".to_string(),
                parent_id: Some("mbox-inbox".to_string()),
                role: None,
                total_emails: 0,
                unread_emails: 0,
                sort_order: 0,
            },
        ];
        assert_eq!(
            resolve_mailbox_id("INBOX/Alerts", &mailboxes),
            Some("mbox-alerts".to_string())
        );
    }

    #[test]
    fn test_custom_header_evaluation() {
        let mut email = make_email("e1");
        email.extra.insert(
            "header:X-Spam-Score:asText".to_string(),
            crate::json::Json::Str("5.5".to_string()),
        );

        let toml_str = r#"
[[rule]]
name = "spam check"
[rule.match]
header = "X-Spam-Score"
regex = "^[5-9]"
[rule.actions]
mark_read = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        let rules: Vec<CompiledRule> = config
            .rule
            .into_iter()
            .map(compile_rule)
            .collect::<Result<_, _>>()
            .unwrap();

        assert!(evaluate_condition(&rules[0].condition, &email));
    }

    #[test]
    fn test_invalid_regex_returns_error() {
        let toml_str = r#"
[[rule]]
name = "bad regex"
[rule.match]
header = "From"
regex = "[invalid"
[rule.actions]
mark_read = true
"#;
        let config = RulesConfig::parse(toml_str).unwrap();
        let result = compile_rule(config.rule.into_iter().next().unwrap());
        assert!(result.is_err());
    }

    #[test]
    fn test_missing_header_returns_false() {
        let mut email = make_email("e1");
        email.cc = None;

        let condition = CompiledCondition::Header {
            header: "CC".to_string(),
            regex: UserRegex::compile("anything").unwrap(),
        };
        assert!(!evaluate_condition(&condition, &email));
    }
}
