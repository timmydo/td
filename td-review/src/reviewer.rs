//! Review transport names map to the unchanged roster slots. Shared with
//! td-builder's ready parser through source inclusion.

pub fn slot<'a>(tool: &'a str, model: &str) -> Result<&'a str, String> {
    let (slot, required) = match tool {
        "td-agent-subagent" => ("subagent", &["gpt", "opus"][..]),
        "td-agent-codex" => ("codex", &["gpt"][..]),
        "td-agent-claude" => ("claude", &["opus"][..]),
        "td-agent-agy" => ("agy", &["gemini"][..]),
        _ => return Ok(tool),
    };
    let lower = model.to_ascii_lowercase();
    if !lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|token| required.contains(&token))
    {
        return Err(format!(
            "'{tool}/{model}' requires a {} model family for the {slot} slot",
            required.join("/")
        ));
    }
    Ok(slot)
}
