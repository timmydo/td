//! The shared review record: one independent frontier review at high effort.

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Record {
    pub frontier: bool,
    docs_only: bool,
    high_effort: bool,
    checks: bool,
    malformed: Vec<String>,
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

fn version_part(part: &str) -> bool {
    part.bytes().all(|b| b.is_ascii_digit()) && part.parse::<u64>().is_ok()
}

fn version(text: &str) -> bool {
    text.split('.').all(version_part)
}

fn frontier_reviewer(tool: &str, model: &str) -> bool {
    if tool == "td-agent" {
        return model
            .strip_prefix("anthropic/claude-fable-")
            .is_some_and(version);
    }
    if tool != "subagent" {
        return false;
    }
    if model
        .strip_prefix("openai/")
        .unwrap_or(model)
        .strip_prefix("gpt-")
        .and_then(|m| m.strip_suffix("-astra"))
        .is_some_and(version)
    {
        return true;
    }
    let base = model.strip_prefix("anthropic/").unwrap_or(model);
    let Some(release) = base
        .strip_prefix("claude-fable-")
        .or_else(|| base.strip_prefix("fable-"))
    else {
        return false;
    };
    // Claude Code spells a release like 5.2 as 5-2.
    version(release)
        || release
            .split_once('-')
            .is_some_and(|(major, minor)| version_part(major) && version_part(minor))
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
                if frontier_reviewer(tool.trim(), model.trim()) {
                    record.frontier = true;
                } else {
                    record
                        .malformed
                        .push(format!("invalid frontier reviewer '{value}'"));
                }
            }
            "review-waiver" if value == "docs-only" => record.docs_only = true,
            "review-waiver" => record.malformed.push(format!("unknown waiver '{value}'")),
            "review-effort" if value == "high" => record.high_effort = true,
            "review-effort" => record.malformed.push("review effort must be high".into()),
            "checks" if !value.is_empty() => record.checks = true,
            "checks" => record.malformed.push("empty Checks trailer".into()),
            _ => {}
        }
    }
    record
}

pub fn problems(message: &str, paths: &[String]) -> Vec<String> {
    let record = parse(message);
    let mut out = record.malformed;
    if record.docs_only {
        if paths.is_empty() {
            out.push("waived as docs-only, but no paths could be read".into());
        }
        for path in paths
            .iter()
            .filter(|p| !p.to_ascii_lowercase().ends_with(".md"))
        {
            out.push(format!("docs-only waiver touches {path}"));
        }
    } else if !record.frontier {
        out.push("no frontier subagent review (GPT Astra or Claude Fable)".into());
    } else if !record.high_effort {
        out.push("missing Review-effort: high".into());
    }
    if !record.checks {
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
                    ["reviewed-by", "review-waiver", "review-effort", "checks"]
                        .contains(&key.to_ascii_lowercase().as_str())
                })
        }) {
            out.push("review trailers do not close the message".into());
        }
    }
    out
}
