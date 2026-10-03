//! The conversation tools (DESIGN.md §3, §12): the first tools a model is
//! given, none of which touches a file. Their definitions are fixed text
//! in the request prefix (§13); a call's arguments are parsed here under
//! a bound, each refused by name rather than guessed at, and the rules of
//! which conversation may read or message which are here too, since the
//! window checks a message again with them.
//!
//! - `todo_write` replaces the conversation's todo list whole.
//! - `history_search` and `history_read` reach a conversation's whole log
//!   (`history`).
//! - `conversations` lists the conversations; `send_message` and `report`
//!   send one a message, which the window routes.
//!
//! The orchestrator has all of them but `report`, which is a message to
//! the orchestrator itself. Every read or message §11 makes a crossing is
//! refused until the crossings are decided (increment 13). There are no
//! workspaces yet, so every conversation outside one counts as a
//! workspace of its own: the orchestrator reads and messages anyone, any
//! conversation messages the orchestrator, and nothing else crosses.

use crate::history::Searchable;
use crate::store::{Id, Role, Status, TodoItem};
use td_json::Json;

/// The most a call's arguments may run to, as the model wrote them.
pub const MAX_ARGUMENTS: usize = 256 * 1024;
/// The most items a todo list holds, and the most bytes each item's text.
pub const MAX_TODO_ITEMS: usize = 50;
pub const MAX_TODO_BYTES: usize = 500;
/// The most a message between conversations, or a report's summary, may
/// run to (DESIGN.md §3).
pub const MAX_MESSAGE: usize = 32 * 1024;
/// The most messages a receiver holds undelivered.
pub const MAX_UNDELIVERED: usize = 16;
/// `history_search`'s hits: by default, and at most.
pub const SEARCH_LIMIT: usize = 20;
pub const MAX_SEARCH_LIMIT: usize = 100;
/// The longest query taken.
pub const MAX_QUERY: usize = 1024;
/// `history_read`'s page: events and bytes, by default and at most.
pub const READ_COUNT: usize = 20;
pub const MAX_READ_COUNT: usize = 100;
pub const READ_BYTES: usize = 32 * 1024;
pub const MAX_READ_BYTES: usize = 256 * 1024;
/// A `report`'s statuses.
pub const REPORT_STATUSES: [&str; 3] = ["in_progress", "done", "blocked"];

/// A conversation tool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tool {
    TodoWrite,
    HistorySearch,
    HistoryRead,
    Conversations,
    SendMessage,
    Report,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Self::TodoWrite => "todo_write",
            Self::HistorySearch => "history_search",
            Self::HistoryRead => "history_read",
            Self::Conversations => "conversations",
            Self::SendMessage => "send_message",
            Self::Report => "report",
        }
    }

    /// The tools a conversation of `role` has, in the order the prefix
    /// defines them.
    pub fn of(role: Role) -> &'static [Self] {
        match role {
            Role::Orchestrator => &[
                Self::Conversations,
                Self::SendMessage,
                Self::HistorySearch,
                Self::HistoryRead,
                Self::TodoWrite,
            ],
            Role::Conversation => &[
                Self::TodoWrite,
                Self::HistorySearch,
                Self::HistoryRead,
                Self::Conversations,
                Self::SendMessage,
                Self::Report,
            ],
        }
    }

    fn find(role: Role, name: &str) -> Option<Self> {
        Self::of(role).iter().copied().find(|t| t.name() == name)
    }
}

/// A JSON Schema property.
fn property(kind: &str, description: &str) -> Json {
    Json::Obj(vec![
        ("type".into(), Json::Str(kind.into())),
        ("description".into(), Json::Str(description.into())),
    ])
}

/// A property whose value is one of `words`.
fn one_of(description: &str, words: &[&str]) -> Json {
    Json::Obj(vec![
        ("type".into(), Json::Str("string".into())),
        (
            "enum".into(),
            Json::Arr(words.iter().map(|w| Json::Str((*w).into())).collect()),
        ),
        ("description".into(), Json::Str(description.into())),
    ])
}

/// An integer property between `least` and `most`.
fn integer(description: &str, least: u64, most: Option<u64>) -> Json {
    let mut pairs = vec![
        ("type".into(), Json::Str("integer".into())),
        ("minimum".into(), Json::from(least)),
    ];
    if let Some(most) = most {
        pairs.push(("maximum".into(), Json::from(most)));
    }
    pairs.push(("description".into(), Json::Str(description.into())));
    Json::Obj(pairs)
}

/// An object schema of `properties`, `required` of them, and no others.
fn schema(properties: Vec<(&str, Json)>, required: &[&str]) -> Json {
    Json::Obj(vec![
        ("type".into(), Json::Str("object".into())),
        (
            "properties".into(),
            Json::Obj(
                properties
                    .into_iter()
                    .map(|(name, value)| (name.to_string(), value))
                    .collect(),
            ),
        ),
        (
            "required".into(),
            Json::Arr(required.iter().map(|r| Json::Str((*r).into())).collect()),
        ),
        ("additionalProperties".into(), Json::Bool(false)),
    ])
}

const CONVERSATION_PROPERTY: &str = "Another conversation's id, from `conversations`, or `orchestrator`. This conversation's own log when left out.";

/// One tool's definition as the request carries it.
fn definition(tool: Tool, role: Role) -> Json {
    let (description, parameters) = match tool {
        Tool::TodoWrite => (
            "Replace this conversation's todo list with the items given, whole. Use it for work of three or more steps: write the plan, keep exactly one item in_progress while you work on it, and mark items done or cancelled as they end. The person sees the list above the composer. At most 50 items of at most 500 bytes of text each, and at most one in_progress. An empty list clears it.".to_string(),
            schema(
                vec![(
                    "items",
                    Json::Obj(vec![
                        ("type".into(), Json::Str("array".into())),
                        ("maxItems".into(), Json::from(MAX_TODO_ITEMS as u64)),
                        (
                            "items".into(),
                            schema(
                                vec![
                                    ("content", property("string", "What the step is, one line.")),
                                    (
                                        "status",
                                        one_of(
                                            "The step's state.",
                                            &Status::ALL.map(Status::word),
                                        ),
                                    ),
                                ],
                                &["content", "status"],
                            ),
                        ),
                        ("description".into(), Json::Str("The whole list, in order.".into())),
                    ]),
                )],
                &["items"],
            ),
        ),
        Tool::HistorySearch => (
            "Search a conversation's whole log, including what has left your context, for events whose text contains every term of the query, case-insensitively, newest first. Each hit gives the event's sequence number, kind, time and an excerpt around the first match; read the whole event with history_read. An approval shows only its outcome and who decided it.".to_string(),
            schema(
                vec![
                    ("query", property("string", "Terms separated by spaces; an event matches when its text contains all of them.")),
                    ("conversation", property("string", CONVERSATION_PROPERTY)),
                    (
                        "kinds",
                        Json::Obj(vec![
                            ("type".into(), Json::Str("array".into())),
                            ("items".into(), one_of("A kind of event.", &Searchable::ALL.map(Searchable::word))),
                            ("description".into(), Json::Str("Only events of these kinds; every kind when left out.".into())),
                        ]),
                    ),
                    ("limit", integer("The most hits returned; 20 when left out.", 1, Some(MAX_SEARCH_LIMIT as u64))),
                ],
                &["query"],
            ),
        ),
        Tool::HistoryRead => (
            "Read a conversation's log as text, from sequence number `from`, starting `offset` bytes into that first event, at most `count` events and `max_bytes` bytes. A tool result comes back whole as the log keeps it, paged rather than cut. The page ends with the cursor to go on from, `from` and `offset`, or says the log ends there; a page never splits a character. An approval shows only its outcome and who decided it.".to_string(),
            schema(
                vec![
                    ("conversation", property("string", CONVERSATION_PROPERTY)),
                    ("from", integer("The sequence number of the first event to read.", 1, None)),
                    ("offset", integer("Bytes into the first event to start at, from a cursor; 0 when left out.", 0, None)),
                    ("count", integer("The most events in the page; 20 when left out.", 1, Some(MAX_READ_COUNT as u64))),
                    ("max_bytes", integer("The most bytes in the page; 32768 when left out.", 1, Some(MAX_READ_BYTES as u64))),
                ],
                &["from"],
            ),
        ),
        Tool::Conversations => (
            match role {
                Role::Orchestrator => "List the conversations td-agent holds: each one's id, role, workspace, state (idle, running, paused, failed), background processes, cost and last activity, with its title and the todo item it has in progress.",
                Role::Conversation => "List the conversations td-agent holds: each one's id, role, workspace, state (idle, running, paused, failed), background processes, cost and last activity. The title and the todo item in progress are shown for this conversation only, since other workspaces' models wrote them.",
            }
            .to_string(),
            schema(Vec::new(), &[]),
        ),
        Tool::SendMessage => (
            "Send a message to another conversation, by its id from `conversations`, or `orchestrator`. It is delivered between that conversation's turns, labelled with this conversation as its source, and starts a turn there; any reply comes back to you the same way, later, so do not wait for one. At most 32 KiB, and a conversation holds at most 16 messages undelivered. Messaging a conversation of another workspace is a crossing and is refused for now.".to_string(),
            schema(
                vec![
                    ("to", property("string", "The receiving conversation's id, or `orchestrator`.")),
                    ("text", property("string", "The message.")),
                ],
                &["to", "text"],
            ),
        ),
        Tool::Report => (
            "Report to the orchestrator: a status and a summary of what was done, what is left, and anything the person must decide. Use it when the work is done, when it is blocked, and at points the orchestrator should know of. It is delivered as a message to the orchestrator.".to_string(),
            schema(
                vec![
                    ("status", one_of("Where the work stands.", &REPORT_STATUSES)),
                    ("summary", property("string", "What was done, what is left, and what needs a decision.")),
                ],
                &["status", "summary"],
            ),
        ),
    };
    Json::Obj(vec![
        ("type".into(), Json::Str("function".into())),
        (
            "function".into(),
            Json::Obj(vec![
                ("name".into(), Json::Str(tool.name().into())),
                ("description".into(), Json::Str(description)),
                ("parameters".into(), parameters),
            ]),
        ),
    ])
}

/// The request prefix of a conversation of `role` (DESIGN.md §13): its
/// tools and their settings, then the messages every request begins
/// with, `messages` last so a request appends to it.
pub fn prefix(role: Role, system: &str) -> String {
    let tools = Tool::of(role)
        .iter()
        .map(|tool| definition(*tool, role))
        .collect();
    Json::Obj(vec![
        ("tools".into(), Json::Arr(tools)),
        (
            "messages".into(),
            Json::Arr(vec![Json::Obj(vec![
                ("role".into(), Json::Str("system".into())),
                ("content".into(), Json::Str(system.into())),
            ])]),
        ),
    ])
    .to_string()
}

/// A conversation a call names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    Orchestrator,
    Id(Id),
}

impl Target {
    fn parse(text: &str, member: &str) -> Result<Self, String> {
        if text == "orchestrator" {
            return Ok(Self::Orchestrator);
        }
        Id::parse(text).map(Self::Id).ok_or_else(|| {
            format!("`{member}` is not a conversation id (32 lowercase hexadecimal digits from `conversations`) or `orchestrator`")
        })
    }
}

/// `history_search`'s arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Search {
    pub query: String,
    pub conversation: Option<Target>,
    pub kinds: Vec<Searchable>,
    pub limit: usize,
}

/// `history_read`'s arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Read {
    pub conversation: Option<Target>,
    pub from: u64,
    pub offset: u64,
    pub count: usize,
    pub max_bytes: usize,
}

/// A call's arguments, parsed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Args {
    Todo(Vec<TodoItem>),
    Search(Search),
    Read(Read),
    Conversations,
    Send { to: Target, text: String },
    Report { status: String, summary: String },
}

/// The members of a call's arguments, every one of them named in
/// `allowed`.
fn members<'a>(
    tool: &str,
    value: &'a Json,
    allowed: &[&str],
) -> Result<&'a [(String, Json)], String> {
    let Json::Obj(members) = value else {
        return Err(format!("{tool}'s arguments are not a JSON object"));
    };
    if let Some((name, _)) = members.iter().find(|(n, _)| !allowed.contains(&n.as_str())) {
        let takes = if allowed.is_empty() {
            "none".to_string()
        } else {
            allowed.join(", ")
        };
        return Err(format!(
            "{tool} takes no `{name}`; its arguments are: {takes}"
        ));
    }
    Ok(members)
}

/// A string member, when present.
fn text<'a>(members: &'a [(String, Json)], name: &str) -> Result<Option<&'a str>, String> {
    match members.iter().find(|(n, _)| n == name).map(|(_, v)| v) {
        None => Ok(None),
        Some(Json::Str(text)) => Ok(Some(text)),
        Some(_) => Err(format!("`{name}` is not a string")),
    }
}

fn required<'a>(members: &'a [(String, Json)], name: &str) -> Result<&'a str, String> {
    text(members, name)?.ok_or_else(|| format!("`{name}` is missing"))
}

/// A whole-number member within `least..=most`, when present.
fn number(
    members: &[(String, Json)],
    name: &str,
    least: u64,
    most: u64,
) -> Result<Option<u64>, String> {
    let Some(value) = members.iter().find(|(n, _)| n == name).map(|(_, v)| v) else {
        return Ok(None);
    };
    let number = value
        .as_u64()
        .ok_or_else(|| format!("`{name}` is not a whole number"))?;
    if number < least || number > most {
        return Err(format!("`{name}` is {number}, outside {least} to {most}"));
    }
    Ok(Some(number))
}

/// A conversation a member names, when present.
fn target(members: &[(String, Json)], name: &str) -> Result<Option<Target>, String> {
    text(members, name)?
        .map(|t| Target::parse(t, name))
        .transpose()
}

/// A message's text: present, not blank, within the bound.
fn message_text(members: &[(String, Json)], name: &str) -> Result<String, String> {
    let text = required(members, name)?;
    if text.trim().is_empty() {
        return Err(format!("`{name}` is empty"));
    }
    if text.len() > MAX_MESSAGE {
        return Err(format!(
            "`{name}` is {} bytes; a message is at most {MAX_MESSAGE}",
            text.len()
        ));
    }
    Ok(text.to_string())
}

/// The todo list a `todo_write` call gives, checked against §12's bounds.
fn todo(given: &[(String, Json)]) -> Result<Vec<TodoItem>, String> {
    let items = match given.iter().find(|(n, _)| n == "items").map(|(_, v)| v) {
        None => return Err("`items` is missing".into()),
        Some(Json::Arr(items)) => items,
        Some(_) => return Err("`items` is not a list".into()),
    };
    if items.len() > MAX_TODO_ITEMS {
        return Err(format!(
            "the list has {} items; a todo list holds at most {MAX_TODO_ITEMS}",
            items.len()
        ));
    }
    let mut list = Vec::with_capacity(items.len());
    for (n, item) in items.iter().enumerate() {
        let at = n + 1;
        let fields = members(&format!("item {at}"), item, &["content", "status"])?;
        let content = required(fields, "content").map_err(|e| format!("item {at}: {e}"))?;
        if content.trim().is_empty() {
            return Err(format!("item {at}'s content is empty"));
        }
        // The window draws an item to a line.
        if content.contains(['\n', '\r']) {
            return Err(format!("item {at}'s content is more than one line"));
        }
        if content.len() > MAX_TODO_BYTES {
            return Err(format!(
                "item {at}'s content is {} bytes; an item is at most {MAX_TODO_BYTES}",
                content.len()
            ));
        }
        let word = required(fields, "status").map_err(|e| format!("item {at}: {e}"))?;
        let status = Status::parse(word).ok_or_else(|| {
            format!(
                "item {at}'s status {word:?} is not one of {}",
                Status::ALL.map(Status::word).join(", ")
            )
        })?;
        list.push(TodoItem {
            content: content.to_string(),
            status,
        });
    }
    let doing = list
        .iter()
        .filter(|i| i.status == Status::InProgress)
        .count();
    if doing > 1 {
        return Err(format!(
            "{doing} items are in_progress; at most one is at a time"
        ));
    }
    Ok(list)
}

/// A call to `name` with `arguments` from a conversation of `role`: its
/// arguments parsed, or why they cannot be, which the call is answered
/// with and nothing is done.
pub fn parse(role: Role, name: &str, arguments: &str) -> Result<Args, String> {
    let tool = Tool::find(role, name).ok_or_else(|| {
        let names: Vec<&str> = Tool::of(role).iter().map(|t| t.name()).collect();
        format!(
            "there is no tool named {name:?}; the tools are {}",
            names.join(", ")
        )
    })?;
    if arguments.len() > MAX_ARGUMENTS {
        return Err(format!(
            "the arguments are {} bytes, past the {MAX_ARGUMENTS}-byte bound",
            arguments.len()
        ));
    }
    // An empty text is the no-arguments call some providers send.
    let arguments = if arguments.trim().is_empty() {
        "{}"
    } else {
        arguments
    };
    let value =
        td_json::parse(arguments).map_err(|e| format!("the arguments are not JSON: {e}"))?;
    let tool_name = tool.name();
    Ok(match tool {
        Tool::TodoWrite => Args::Todo(todo(members(tool_name, &value, &["items"])?)?),
        Tool::HistorySearch => {
            let m = members(
                tool_name,
                &value,
                &["query", "conversation", "kinds", "limit"],
            )?;
            let query = required(m, "query")?;
            if query.split_whitespace().next().is_none() {
                return Err("`query` has no terms".into());
            }
            if query.len() > MAX_QUERY {
                return Err(format!(
                    "`query` is {} bytes; at most {MAX_QUERY}",
                    query.len()
                ));
            }
            let kinds = match m.iter().find(|(n, _)| n == "kinds").map(|(_, v)| v) {
                None => Vec::new(),
                Some(Json::Arr(kinds)) => kinds
                    .iter()
                    .map(|kind| {
                        let word = kind
                            .as_str()
                            .ok_or("`kinds` holds something not a string")?;
                        Searchable::parse(word).ok_or_else(|| {
                            format!(
                                "{word:?} is not a kind; the kinds are {}",
                                Searchable::ALL.map(Searchable::word).join(", ")
                            )
                        })
                    })
                    .collect::<Result<_, String>>()?,
                Some(_) => return Err("`kinds` is not a list".into()),
            };
            Args::Search(Search {
                query: query.to_string(),
                conversation: target(m, "conversation")?,
                kinds,
                limit: number(m, "limit", 1, MAX_SEARCH_LIMIT as u64)?
                    .map_or(SEARCH_LIMIT, |n| n as usize),
            })
        }
        Tool::HistoryRead => {
            let m = members(
                tool_name,
                &value,
                &["conversation", "from", "offset", "count", "max_bytes"],
            )?;
            Args::Read(Read {
                conversation: target(m, "conversation")?,
                from: number(m, "from", 1, u64::MAX)?.ok_or("`from` is missing")?,
                offset: number(m, "offset", 0, u64::MAX)?.unwrap_or(0),
                count: number(m, "count", 1, MAX_READ_COUNT as u64)?
                    .map_or(READ_COUNT, |n| n as usize),
                max_bytes: number(m, "max_bytes", 1, MAX_READ_BYTES as u64)?
                    .map_or(READ_BYTES, |n| n as usize),
            })
        }
        Tool::Conversations => {
            members(tool_name, &value, &[])?;
            Args::Conversations
        }
        Tool::SendMessage => {
            let m = members(tool_name, &value, &["to", "text"])?;
            Args::Send {
                to: target(m, "to")?.ok_or("`to` is missing")?,
                text: message_text(m, "text")?,
            }
        }
        Tool::Report => {
            let m = members(tool_name, &value, &["status", "summary"])?;
            let status = required(m, "status")?;
            if !REPORT_STATUSES.contains(&status) {
                return Err(format!(
                    "`status` {status:?} is not one of {}",
                    REPORT_STATUSES.join(", ")
                ));
            }
            Args::Report {
                status: status.to_string(),
                summary: message_text(m, "summary")?,
            }
        }
    })
}

/// What a crossing check is for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Op {
    Read,
    Message,
}

/// Whether a conversation (`caller`, of `caller_role`) may read or message
/// `target` (of `target_role`) without a crossing (DESIGN.md §3, §11), or
/// why it is refused. Crossings are decided from increment 13 and refused
/// until then; there are no workspaces yet, so every conversation is a
/// workspace of its own.
pub fn crossing(
    caller: &Id,
    caller_role: Role,
    target: &Id,
    target_role: Role,
    op: Op,
) -> Result<(), String> {
    const UNTIL: &str = "crossings are decided by approval from a later increment of td-agent, and until then every one is refused; do not try to reach it another way";
    if caller == target {
        return match op {
            Op::Read => Ok(()),
            Op::Message => Err("a conversation does not send messages to itself".into()),
        };
    }
    if caller_role == Role::Orchestrator {
        return Ok(());
    }
    match (op, target_role) {
        (Op::Message, Role::Orchestrator) => Ok(()),
        (Op::Read, Role::Orchestrator) => Err(format!(
            "reading the orchestrator's log is a crossing for a workspace, since it holds every workspace's reports; {UNTIL}"
        )),
        (Op::Read, Role::Conversation) => Err(format!(
            "reading another workspace's conversation is a crossing, and a conversation outside any workspace counts as a workspace of its own; {UNTIL}"
        )),
        (Op::Message, Role::Conversation) => Err(format!(
            "messaging another workspace's conversation is a crossing, and a conversation outside any workspace counts as a workspace of its own; {UNTIL}. Report to the orchestrator instead"
        )),
    }
}

/// A todo list as a tool result and the window show it.
pub fn todo_text(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "The todo list is empty.".into();
    }
    let done = items.iter().filter(|i| i.status == Status::Done).count();
    let mut out = format!("The todo list has {} items, {done} done:", items.len());
    for item in items {
        out.push('\n');
        out.push_str(mark(item.status));
        out.push(' ');
        out.push_str(&item.content);
    }
    out
}

/// A todo item's mark.
pub fn mark(status: Status) -> &'static str {
    match status {
        Status::Pending => "[ ]",
        Status::InProgress => "[>]",
        Status::Done => "[x]",
        Status::Cancelled => "[-]",
    }
}

/// One conversation as `conversations` lists it. The title and the item
/// in progress are model-written; the rest td-agent writes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Listed {
    pub id: Id,
    pub role: Role,
    pub state: String,
    pub cost: u64,
    pub activity: u64,
    pub title: String,
    pub doing: Option<String>,
}

/// The `conversations` result for `caller`, of `caller_role`: the
/// harness-written fields for every conversation, and the model-written
/// ones only for the caller's own workspace, or for every one to the
/// orchestrator (DESIGN.md §3); `omitted` more were left out.
pub fn listing(caller: &Id, caller_role: Role, entries: &[Listed], omitted: usize) -> String {
    let mut out = format!("{} conversations", entries.len() + omitted);
    if omitted > 0 {
        out.push_str(&format!(
            ", the {} most recently active shown",
            entries.len()
        ));
    }
    out.push_str(match caller_role {
        Role::Orchestrator => ":",
        Role::Conversation => {
            "; titles and items in progress are shown for this conversation only, since other workspaces' models wrote them:"
        }
    });
    for entry in entries {
        let own = &entry.id == caller;
        out.push_str(&format!(
            "\n{} {}{} | workspace none | {} | background 0 | cost {} | active {}",
            entry.id,
            entry.role.word(),
            if own { " (this conversation)" } else { "" },
            entry.state,
            crate::cost::show(entry.cost),
            crate::history::utc(entry.activity),
        ));
        if own || caller_role == Role::Orchestrator {
            out.push_str(&format!("\n  title: {}", entry.title));
            if let Some(doing) = &entry.doing {
                out.push_str(&format!("\n  in progress: {doing}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;

    fn id(n: u8) -> Id {
        Id::parse(&format!("{n:032x}")).unwrap()
    }

    #[test]
    fn the_prefix_defines_each_roles_tools_before_its_messages() {
        for role in [Role::Orchestrator, Role::Conversation] {
            let text = prefix(role, "system text");
            assert!(text.ends_with("]}"), "{text}");
            let value = td_json::parse(&text).unwrap();
            let names: Vec<&str> = value
                .get("tools")
                .unwrap()
                .as_arr()
                .unwrap()
                .iter()
                .map(|t| t.get_path(&["function", "name"]).unwrap().as_str().unwrap())
                .collect();
            let expected: Vec<&str> = Tool::of(role).iter().map(|t| t.name()).collect();
            assert_eq!(names, expected);
            // `require_parameters` would route a request carrying either
            // to no endpoint of a model that does not list it; `auto` is
            // the default, and parallel calls need no asking.
            assert!(value.get("tool_choice").is_none());
            assert!(value.get("parallel_tool_calls").is_none());
            let Json::Obj(members) = &value else {
                panic!("not an object")
            };
            assert_eq!(members.last().unwrap().0, "messages");
            // The same text every time: the prefix is fixed.
            assert_eq!(text, prefix(role, "system text"));
        }
        assert!(!Tool::of(Role::Orchestrator).contains(&Tool::Report));
    }

    #[test]
    fn a_todo_list_is_held_to_its_bounds() {
        let item = |content: &str, status: &str| {
            format!(r#"{{"content":"{content}","status":"{status}"}}"#)
        };
        let call = |items: &[String]| {
            parse(
                Role::Conversation,
                "todo_write",
                &format!(r#"{{"items":[{}]}}"#, items.join(",")),
            )
        };
        let Args::Todo(list) = call(&[
            item("plan", "done"),
            item("build", "in_progress"),
            item("test", "pending"),
            item("skip", "cancelled"),
        ])
        .unwrap() else {
            panic!("not a todo")
        };
        assert_eq!(list.len(), 4);
        assert_eq!(list[1].status, Status::InProgress);
        assert_eq!(
            todo_text(&list),
            "The todo list has 4 items, 1 done:\n[x] plan\n[>] build\n[ ] test\n[-] skip"
        );
        assert_eq!(call(&[]).unwrap(), Args::Todo(Vec::new()));
        let fifty: Vec<String> = (0..MAX_TODO_ITEMS)
            .map(|n| item(&n.to_string(), "pending"))
            .collect();
        assert!(call(&fifty).is_ok());
        let fifty_one: Vec<String> = (0..=MAX_TODO_ITEMS)
            .map(|n| item(&n.to_string(), "pending"))
            .collect();
        assert!(call(&fifty_one).unwrap_err().contains("at most 50"));
        assert!(call(&[item(&"x".repeat(MAX_TODO_BYTES), "pending")]).is_ok());
        let long = call(&[item(&"x".repeat(MAX_TODO_BYTES + 1), "pending")]).unwrap_err();
        assert!(long.contains("501 bytes"), "{long}");
        let two = call(&[item("a", "in_progress"), item("b", "in_progress")]).unwrap_err();
        assert!(two.contains("at most one"), "{two}");
        let status = call(&[item("a", "started")]).unwrap_err();
        assert!(
            status.contains("pending, in_progress, done, cancelled"),
            "{status}"
        );
        assert!(call(&[item(" ", "pending")]).unwrap_err().contains("empty"));
        assert!(call(&[item("a\\nb", "pending")])
            .unwrap_err()
            .contains("more than one line"));
        let extra = parse(
            Role::Conversation,
            "todo_write",
            r#"{"items":[{"content":"a","status":"pending","id":"1"}]}"#,
        )
        .unwrap_err();
        assert!(extra.contains("takes no `id`"), "{extra}");
    }

    #[test]
    fn arguments_that_do_not_parse_are_refused_by_name() {
        for (arguments, said) in [
            (r#"{"items": ["#, "not JSON"),
            ("[1]", "not a JSON object"),
            (r#"{"items":"a"}"#, "not a list"),
            (r#"{}"#, "`items` is missing"),
            (r#"{"items":[],"more":1}"#, "takes no `more`"),
        ] {
            let e = parse(Role::Conversation, "todo_write", arguments).unwrap_err();
            assert!(e.contains(said), "{arguments}: {e}");
        }
        let big = format!(r#"{{"items":[],"x":"{}"}}"#, "a".repeat(MAX_ARGUMENTS));
        assert!(parse(Role::Conversation, "todo_write", &big)
            .unwrap_err()
            .contains("bound"));
        let unknown = parse(Role::Conversation, "read_file", "{}").unwrap_err();
        assert!(unknown.contains("no tool named \"read_file\""), "{unknown}");
        // The orchestrator has no report.
        assert!(parse(
            Role::Orchestrator,
            "report",
            r#"{"status":"done","summary":"x"}"#
        )
        .is_err());
        // No arguments at all is an empty object.
        assert_eq!(
            parse(Role::Conversation, "conversations", "").unwrap(),
            Args::Conversations
        );
        assert!(parse(Role::Conversation, "conversations", r#"{"all":true}"#).is_err());
        for (arguments, said) in [
            (r#"{"query":"  "}"#, "no terms"),
            (r#"{"query":"a","limit":0}"#, "outside 1 to 100"),
            (r#"{"query":"a","limit":101}"#, "outside 1 to 100"),
            (r#"{"query":"a","limit":"5"}"#, "not a whole number"),
            (r#"{"query":"a","kinds":["mail"]}"#, "not a kind"),
            (
                r#"{"query":"a","conversation":"bob"}"#,
                "not a conversation id",
            ),
        ] {
            let e = parse(Role::Conversation, "history_search", arguments).unwrap_err();
            assert!(e.contains(said), "{arguments}: {e}");
        }
        for (arguments, said) in [
            (r#"{}"#, "`from` is missing"),
            (r#"{"from":0}"#, "outside 1"),
            (r#"{"from":1,"count":101}"#, "outside 1 to 100"),
            (r#"{"from":1,"max_bytes":262145}"#, "outside 1 to 262144"),
        ] {
            let e = parse(Role::Conversation, "history_read", arguments).unwrap_err();
            assert!(e.contains(said), "{arguments}: {e}");
        }
        for (arguments, said) in [
            (r#"{"to":"orchestrator"}"#, "`text` is missing"),
            (r#"{"to":"orchestrator","text":" "}"#, "empty"),
            (r#"{"text":"hi"}"#, "`to` is missing"),
        ] {
            let e = parse(Role::Conversation, "send_message", arguments).unwrap_err();
            assert!(e.contains(said), "{arguments}: {e}");
        }
        let long = format!(
            r#"{{"to":"orchestrator","text":"{}"}}"#,
            "x".repeat(MAX_MESSAGE + 1)
        );
        assert!(parse(Role::Conversation, "send_message", &long)
            .unwrap_err()
            .contains("at most 32768"));
        let status = parse(
            Role::Conversation,
            "report",
            r#"{"status":"finished","summary":"x"}"#,
        )
        .unwrap_err();
        assert!(status.contains("in_progress, done, blocked"), "{status}");
    }

    #[test]
    fn arguments_parse_with_their_defaults() {
        assert_eq!(
            parse(
                Role::Conversation,
                "history_search",
                r#"{"query":"build failed"}"#
            )
            .unwrap(),
            Args::Search(Search {
                query: "build failed".into(),
                conversation: None,
                kinds: Vec::new(),
                limit: SEARCH_LIMIT,
            })
        );
        let other = format!("{}", id(7));
        assert_eq!(
            parse(
                Role::Orchestrator,
                "history_read",
                &format!(
                    r#"{{"conversation":"{other}","from":3,"offset":10,"count":5,"max_bytes":100}}"#
                )
            )
            .unwrap(),
            Args::Read(Read {
                conversation: Some(Target::Id(id(7))),
                from: 3,
                offset: 10,
                count: 5,
                max_bytes: 100,
            })
        );
        assert_eq!(
            parse(Role::Conversation, "history_read", r#"{"from":1}"#).unwrap(),
            Args::Read(Read {
                conversation: None,
                from: 1,
                offset: 0,
                count: READ_COUNT,
                max_bytes: READ_BYTES,
            })
        );
        assert_eq!(
            parse(
                Role::Conversation,
                "report",
                r#"{"status":"done","summary":"built"}"#
            )
            .unwrap(),
            Args::Report {
                status: "done".into(),
                summary: "built".into()
            }
        );
        assert_eq!(
            parse(
                Role::Orchestrator,
                "send_message",
                &format!(r#"{{"to":"{other}","text":"go"}}"#)
            )
            .unwrap(),
            Args::Send {
                to: Target::Id(id(7)),
                text: "go".into()
            }
        );
    }

    #[test]
    fn crossings_are_refused_until_they_are_decided() {
        let (orchestrator, a, b) = (id(1), id(2), id(3));
        use Role::{Conversation as C, Orchestrator as O};
        // Its own log, and anything for the orchestrator.
        assert!(crossing(&a, C, &a, C, Op::Read).is_ok());
        assert!(crossing(&orchestrator, O, &a, C, Op::Read).is_ok());
        assert!(crossing(&orchestrator, O, &a, C, Op::Message).is_ok());
        assert!(crossing(&a, C, &orchestrator, O, Op::Message).is_ok());
        // Everything else crosses.
        let e = crossing(&a, C, &orchestrator, O, Op::Read).unwrap_err();
        assert!(
            e.contains("reading the orchestrator's log is a crossing"),
            "{e}"
        );
        let e = crossing(&a, C, &b, C, Op::Read).unwrap_err();
        assert!(
            e.contains("another workspace's conversation is a crossing"),
            "{e}"
        );
        assert!(e.contains("refused"), "{e}");
        let e = crossing(&a, C, &b, C, Op::Message).unwrap_err();
        assert!(e.contains("messaging another workspace's"), "{e}");
        assert!(crossing(&a, C, &a, C, Op::Message)
            .unwrap_err()
            .contains("itself"));
    }

    #[test]
    fn the_listing_shows_model_written_fields_to_their_own_and_the_orchestrator() {
        let entry = |n: u8, role: Role| Listed {
            id: id(n),
            role,
            state: "idle".into(),
            cost: 0,
            activity: 0,
            title: format!("title {n}"),
            doing: Some(format!("doing {n}")),
        };
        let entries = [
            entry(1, Role::Orchestrator),
            entry(2, Role::Conversation),
            entry(3, Role::Conversation),
        ];
        let mine = listing(&id(2), Role::Conversation, &entries, 0);
        assert!(
            mine.contains("title 2") && mine.contains("doing 2"),
            "{mine}"
        );
        for hidden in ["title 1", "doing 1", "title 3", "doing 3"] {
            assert!(!mine.contains(hidden), "{hidden} in {mine}");
        }
        assert!(mine.contains(&format!("{} orchestrator", id(1))), "{mine}");
        assert!(
            mine.contains(&format!("{} conversation (this conversation)", id(2))),
            "{mine}"
        );
        let all = listing(&id(1), Role::Orchestrator, &entries, 2);
        for shown in ["title 1", "title 2", "doing 3"] {
            assert!(all.contains(shown), "{shown} not in {all}");
        }
        assert!(
            all.starts_with("5 conversations, the 3 most recently active shown:"),
            "{all}"
        );
    }
}
