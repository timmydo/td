//! The prompt texts (DESIGN.md §13), plain files under `td-agent/prompt/`
//! compiled in. They are program source, named `.txt` so that no
//! documentation-only waiver covers them, and reviewed like code.
//!
//! A conversation's request prefix is its role's tool definitions and its
//! static text as the one system message every request begins with
//! (`tools::prefix`), written to the conversation's `prefix` file at
//! creation. There is no environment block and no project instructions
//! yet, which later increments add to it.

use crate::json::Json;
use crate::store::Role;

pub const CONVERSATION: &str = include_str!("../prompt/conversation.txt");
pub const ORCHESTRATOR: &str = include_str!("../prompt/orchestrator.txt");
/// The system text of a title request (DESIGN.md §13).
pub const TITLE: &str = include_str!("../prompt/title.txt");

/// One message's JSON text.
pub fn message(role: &str, content: &str) -> String {
    Json::Obj(vec![
        ("role".into(), Json::Str(role.into())),
        ("content".into(), Json::Str(content.into())),
    ])
    .to_string()
}

/// The prefix a conversation of `role` begins with: a JSON object of its
/// tools and the messages every request starts with.
pub fn prefix(role: Role) -> String {
    let text = match role {
        Role::Orchestrator => ORCHESTRATOR,
        Role::Conversation => CONVERSATION,
    };
    crate::tools::prefix(role, text.trim_end())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn a_prefix_holds_the_tools_and_one_system_message() {
        for role in [Role::Orchestrator, Role::Conversation] {
            let prefix = prefix(role);
            let value = crate::json::parse(&prefix).unwrap();
            assert!(value.get("tools").is_some());
            let messages = value.get("messages").unwrap().as_arr().unwrap();
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].get("role").unwrap().as_str(), Some("system"));
            let content = messages[0].get("content").unwrap().as_str().unwrap();
            assert!(content.starts_with("You are td-agent"), "{content}");
            assert!(!content.ends_with('\n'));
        }
        assert_ne!(prefix(Role::Orchestrator), prefix(Role::Conversation));
    }
}
