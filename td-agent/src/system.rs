//! The system context, as the transcript shows it (DESIGN.md §4): a
//! conversation's request prefix (§6, §13) read back into its system
//! prompt and its tools' names and descriptions, one message folded to
//! its header until opened, whose copy gives the prefix's exact bytes.

use std::sync::Arc;

use td_json::Json;
use td_ui::messages::{Message, Tone, MAX_SECTIONS};

/// The system message's header.
pub const HEADER: &str = "system";
/// Its status when a `prefix` event replaced the one before.
pub const REPLACED: &str = "replaced from here";

/// The transcript's message for `prefix`, or none for an empty one (a
/// conversation whose first request has not yet written it). A prefix
/// this window cannot read is shown by its bytes alone.
pub fn message(prefix: &str, replaced: bool) -> Result<Option<Message>, String> {
    if prefix.is_empty() {
        return Ok(None);
    }
    let parsed = td_json::parse(prefix).ok();
    // An object of tools and messages, or, as increment 7 wrote it, an
    // array of messages.
    let (messages, tools) = match &parsed {
        Some(value @ Json::Obj(_)) => (
            value.get("messages").and_then(Json::as_arr),
            value.get("tools").and_then(Json::as_arr),
        ),
        Some(Json::Arr(items)) => (Some(items.as_slice()), None),
        _ => (None, None),
    };
    let mut message = Message::new(HEADER).map_err(|e| e.to_string())?;
    if replaced {
        message = message
            .status(REPLACED, Tone::Neutral)
            .map_err(|e| e.to_string())?;
    }
    let messages = messages.unwrap_or_default();
    // Room for the tools and a line saying how many more there are.
    let shown = MAX_SECTIONS.saturating_sub(2);
    for item in messages.iter().take(shown) {
        let title = match item.get("role").and_then(Json::as_str) {
            Some("system") => "system prompt",
            Some("developer") => "developer prompt",
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => "message",
        };
        let text = match item.get("content") {
            Some(Json::Str(text)) => text.clone(),
            Some(Json::Null) | None => String::new(),
            Some(other) => other.to_string(),
        };
        message = message
            .section(title, &text, false)
            .map_err(|e| e.to_string())?;
    }
    if messages.len() > shown {
        message = message
            .text(&format!(
                "{} more messages; copy gives them all",
                messages.len() - shown
            ))
            .map_err(|e| e.to_string())?;
    }
    if let Some(tools) = tools.filter(|tools| !tools.is_empty()) {
        let text = tools
            .iter()
            .map(|tool| {
                let function = tool.get("function").unwrap_or(tool);
                let name = function.get("name").and_then(Json::as_str).unwrap_or("?");
                match function.get("description").and_then(Json::as_str) {
                    Some(description) => format!("{name}: {description}"),
                    None => name.to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        message = message
            .section(&format!("tools ({})", tools.len()), &text, true)
            .map_err(|e| e.to_string())?;
    }
    if message.sections() == 0 {
        message = message
            .text("nothing in this prefix is shown here; copy gives its bytes")
            .map_err(|e| e.to_string())?;
    }
    message
        .source(Arc::from(prefix))
        .map(|message| Some(message.collapsed(true)))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_prefix_shows_its_system_prompt_and_tools_folded() {
        {
            let prefix = crate::prompt::prefix(0);
            let message = message(&prefix, false).unwrap().unwrap();
            assert_eq!(message.label(), HEADER);
            assert!(message.is_collapsed());
            assert_eq!(message.sections(), 2);
            let system = td_json::parse(&prefix).unwrap();
            let content = system
                .get_path(&["messages"])
                .and_then(|m| m.index(0))
                .and_then(|m| m.get("content"))
                .and_then(Json::as_str)
                .unwrap();
            assert_eq!(message.section_text(0), Some(content));
            let tools = message.section_text(1).unwrap();
            for tool in crate::tools::Tool::all(false) {
                assert!(tools.contains(&format!("{}: ", tool.name())), "{tools}");
            }
        }
    }

    #[test]
    fn an_older_or_unread_prefix_is_still_shown_and_an_empty_one_is_not() {
        assert!(message("", false).unwrap().is_none());
        let older = r#"[{"role":"system","content":"be brief"}]"#;
        let shown = message(older, true).unwrap().unwrap();
        assert_eq!(shown.sections(), 1);
        assert_eq!(shown.section_text(0), Some("be brief"));
        for unread in ["not json", r#"{"messages":[],"tools":[]}"#] {
            let unread = message(unread, false).unwrap().unwrap();
            assert_eq!(unread.sections(), 1);
            assert!(unread
                .section_text(0)
                .unwrap()
                .contains("copy gives its bytes"));
        }
        let many = format!(
            "[{}]",
            vec![r#"{"role":"user","content":"u"}"#; 20].join(",")
        );
        let many = message(&many, false).unwrap().unwrap();
        assert_eq!(many.sections(), MAX_SECTIONS - 1);
        assert_eq!(
            many.section_text(MAX_SECTIONS - 2),
            Some("6 more messages; copy gives them all")
        );
        let odd = r#"[{"role":"developer","content":"d"},{"role":"assistant","content":null}]"#;
        let odd = message(odd, false).unwrap().unwrap();
        assert_eq!(odd.sections(), 2);
        assert_eq!(odd.section_text(1), Some(""));
    }
}
