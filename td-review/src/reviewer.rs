//! The shared review record: one independent Opus and GPT Sol opinion.

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Record {
    pub reviewers: Vec<(String, String)>,
    pub waivers: Vec<(String, String)>,
    pub checks: Option<String>,
    pub malformed: Vec<String>,
}

fn trailer(line: &str) -> bool {
    line.split_once(':').is_some_and(|(key, _)| {
        key.starts_with(|c: char| c.is_ascii_alphabetic())
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

fn block(message: &str) -> Vec<&str> {
    let lines: Vec<&str> = message.lines().collect();
    let mut end = lines.len();
    while end > 0 && lines.get(end - 1).is_some_and(|l| l.trim().is_empty()) {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && lines.get(start - 1).is_some_and(|l| trailer(l)) {
        start -= 1;
    }
    lines
        .get(start..end)
        .map(<[&str]>::to_vec)
        .unwrap_or_default()
}

fn version(text: &str) -> bool {
    !text.is_empty()
        && text
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

fn family(model: &str) -> Option<&'static str> {
    let lower = model.to_ascii_lowercase();
    let base = lower
        .strip_prefix("anthropic/")
        .or_else(|| lower.strip_prefix("openai/"))
        .unwrap_or(&lower);
    if base
        .strip_prefix("claude-opus-")
        .or_else(|| base.strip_prefix("opus-"))
        .is_some_and(version)
    {
        return Some("opus");
    }
    if base
        .strip_prefix("gpt-")
        .and_then(|m| m.strip_suffix("-sol"))
        .is_some_and(version)
    {
        return Some("gpt-sol");
    }
    None
}

fn receipt_family(transport: &str, model: &str) -> Option<&'static str> {
    family(model).or_else(|| {
        // Prior CLI rosters admitted Astra as the GPT family's opinion.
        // New harness and td-agent receipts must name GPT Sol.
        let lower = model.to_ascii_lowercase();
        let base = lower.strip_prefix("openai/").unwrap_or(&lower);
        if transport != "legacy-cli" {
            return None;
        }
        if base
            .strip_prefix("claude-opus-")
            .or_else(|| base.strip_prefix("opus-"))
            .is_some_and(|v| v.split('-').all(version))
        {
            return Some("opus");
        }
        base.strip_prefix("gpt-")
            .and_then(|m| m.strip_suffix("-astra"))
            .is_some_and(version)
            .then_some("gpt-sol")
    })
}

fn transport(tool: &str) -> Option<&'static str> {
    match tool {
        "subagent" | "harness" | "td-agent-subagent" => Some("subagent"),
        "td-agent" | "td-agent-codex" | "td-agent-claude" => Some("td-agent"),
        // Existing receipts stay readable; these are no longer invocation paths.
        "claude" | "codex" => Some("legacy-cli"),
        "agy" | "td-agent-agy" => Some("retired"),
        _ => None,
    }
}

pub fn parse(message: &str) -> Record {
    let mut record = Record::default();
    for line in block(message) {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.to_ascii_lowercase().as_str() {
            "reviewed-by" => {
                let Some((tool, model)) = value.split_once('/') else {
                    record
                        .malformed
                        .push(format!("malformed Reviewed-by: {value}"));
                    continue;
                };
                let tool = tool.trim();
                let model = model.trim();
                let Some(normalized) = transport(tool) else {
                    record.malformed.push(format!("unknown reviewer '{tool}'"));
                    continue;
                };
                if normalized == "retired" {
                    continue;
                }
                let found = receipt_family(normalized, model);
                let bound = match tool {
                    "codex" | "td-agent-codex" => Some("gpt-sol"),
                    "claude" | "td-agent-claude" => Some("opus"),
                    _ => None,
                };
                if found.is_none() || bound.is_some_and(|expected| found != Some(expected)) {
                    record.malformed.push(format!("'{tool}/{model}' is not a concrete reviewer of the required frontier model"));
                    continue;
                }
                let reviewer = (normalized.to_string(), model.to_string());
                if !record.reviewers.contains(&reviewer) {
                    record.reviewers.push(reviewer);
                }
            }
            "review-waiver" => {
                let (who, why) = value.split_once(char::is_whitespace).unwrap_or((value, ""));
                let who = who.trim_end_matches([':', '-', ',']);
                let who = match who {
                    "claude" => "opus",
                    "codex" => "gpt-sol",
                    "agy" => continue,
                    other => other,
                };
                record.waivers.push((
                    who.to_string(),
                    why.trim_start_matches(|c: char| {
                        c == '—' || c == '-' || c == ':' || c.is_whitespace()
                    })
                    .to_string(),
                ));
            }
            "checks" if !value.is_empty() => record.checks = Some(value.to_string()),
            "checks" => record.malformed.push("empty Checks trailer".into()),
            // Historical quota provenance has no eligibility effect.
            "review-fallback" => {}
            _ => {}
        }
    }
    record
}

fn approved(why: &str) -> bool {
    let lower = why.to_ascii_lowercase();
    let Some((reason, name)) = lower.split_once("approved by ") else {
        return false;
    };
    let name = name.trim();
    !reason
        .trim_matches(|c: char| c.is_whitespace() || c == ';' || c == ',')
        .is_empty()
        && !name.is_empty()
        && !name.contains(['<', '>'])
        && !matches!(name, "human" | "name" | "who" | "approver")
}

pub fn problems(message: &str, paths: &[String]) -> Vec<String> {
    let record = parse(message);
    let mut out = record.malformed.clone();
    let docs_only = record.waivers.iter().any(|(who, _)| who == "docs-only");
    if docs_only {
        if paths.is_empty() {
            out.push("waived as docs-only, but no paths could be read".into());
        }
        for path in paths
            .iter()
            .filter(|p| !p.to_ascii_lowercase().ends_with(".md"))
        {
            out.push(format!("docs-only waiver touches {path}"));
        }
    }
    if record
        .waivers
        .iter()
        .filter(|(who, _)| who != "docs-only")
        .count()
        > 1
    {
        out.push("at most one frontier reviewer may be waived at once".into());
    }
    for (who, why) in &record.waivers {
        if who == "docs-only" {
            continue;
        }
        if !["opus", "gpt-sol"].contains(&who.as_str()) {
            out.push(format!("unknown waiver '{who}'"));
        }
        if !approved(why) {
            out.push(format!(
                "waiver '{who}' needs a reason and its human approver"
            ));
        }
    }
    if !docs_only {
        for wanted in ["opus", "gpt-sol"] {
            if !record
                .reviewers
                .iter()
                .any(|(transport, model)| receipt_family(transport, model) == Some(wanted))
                && !record
                    .waivers
                    .iter()
                    .any(|(who, why)| who == wanted && approved(why))
            {
                out.push(format!("no {wanted} review"));
            }
        }
    }
    if record.checks.is_none() {
        out.push("no Checks trailer".into());
    }
    if !out.is_empty() {
        let closing = block(message);
        let final_start = message
            .trim_end()
            .lines()
            .count()
            .saturating_sub(closing.len());
        if message.lines().take(final_start).any(|line| {
            trailer(line)
                && line.split_once(':').is_some_and(|(key, _)| {
                    ["reviewed-by", "review-waiver", "checks"]
                        .contains(&key.to_ascii_lowercase().as_str())
                })
        }) {
            out.push("review trailers do not close the message".into());
        }
    }
    out
}
