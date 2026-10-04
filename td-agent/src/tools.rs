//! The tools (DESIGN.md §3, §12): the conversation tools every
//! conversation has, and the workspace tools one in a workspace has too,
//! which the tool host runs in its jail. Their definitions are fixed text
//! in the request prefix (§13); a call's arguments are parsed here under
//! a bound, each refused by name rather than guessed at, and the rules of
//! which conversation may read or message which are here too, since the
//! window checks a message again with them. So is the card that asks the
//! human about a workspace tool's call (§11).
//!
//! - `todo_write` replaces the conversation's todo list whole.
//! - `history_search` and `history_read` reach a conversation's whole log
//!   (`history`).
//! - `conversations` lists the conversations; `send_message` sends one a
//!   message, which the window routes.
//! - `read_file`, `write_file`, `edit_file`, `glob`, `grep`, `sed` and
//!   `shell` read and change the workspace (`host::Call`).
//!
//! Every conversation has the same tools: there is no orchestrator. Each
//! counts as a workspace of its own, so reading another's log or
//! messaging another is a crossing, which the human decides on a card
//! (§11) in either mode, and `crossing_card` draws.

use crate::history::Searchable;
use crate::host::Call;
use crate::store::{Id, Status, TodoItem};
use crate::{files, shell};
use td_json::Json;

/// The most a call's arguments may run to, as the model wrote them.
pub const MAX_ARGUMENTS: usize = 256 * 1024;
/// The most items a todo list holds, and the most bytes each item's text.
pub const MAX_TODO_ITEMS: usize = 50;
pub const MAX_TODO_BYTES: usize = 500;
/// The most a message between conversations may run to (DESIGN.md §3).
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

/// A conversation tool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tool {
    TodoWrite,
    HistorySearch,
    HistoryRead,
    Conversations,
    SendMessage,
    ReadFile,
    WriteFile,
    EditFile,
    Glob,
    Grep,
    Sed,
    Shell,
}

/// The tools every conversation has, in the order the prefix defines
/// them.
const CONVERSATION: &[Tool] = &[
    Tool::TodoWrite,
    Tool::HistorySearch,
    Tool::HistoryRead,
    Tool::Conversations,
    Tool::SendMessage,
];

/// The tools a workspace adds (DESIGN.md §12), run by the tool host in
/// the workspace's jail, in the order the prefix defines them.
const WORKSPACE: &[Tool] = &[
    Tool::ReadFile,
    Tool::WriteFile,
    Tool::EditFile,
    Tool::Glob,
    Tool::Grep,
    Tool::Sed,
    Tool::Shell,
];

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Self::TodoWrite => "todo_write",
            Self::HistorySearch => "history_search",
            Self::HistoryRead => "history_read",
            Self::Conversations => "conversations",
            Self::SendMessage => "send_message",
            Self::ReadFile => "read_file",
            Self::WriteFile => "write_file",
            Self::EditFile => "edit_file",
            Self::Glob => "glob",
            Self::Grep => "grep",
            Self::Sed => "sed",
            Self::Shell => "shell",
        }
    }

    /// The tools a conversation has, with a workspace's when it works in
    /// one.
    pub fn all(workspace: bool) -> Vec<Self> {
        let mut tools = CONVERSATION.to_vec();
        if workspace {
            tools.extend_from_slice(WORKSPACE);
        }
        tools
    }

    /// Whether a call to it changes the workspace or runs a command, which
    /// in `ask` mode the human decides first (DESIGN.md §11).
    pub fn acts(self) -> bool {
        matches!(
            self,
            Self::WriteFile | Self::EditFile | Self::Sed | Self::Shell
        )
    }

    fn find(workspace: bool, name: &str) -> Option<Self> {
        Self::all(workspace).into_iter().find(|t| t.name() == name)
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

const CONVERSATION_PROPERTY: &str = "Another conversation's id, from `conversations`; this conversation's own log when left out or empty. The person approves each search or read of another conversation's log before it is made, and may refuse it.";

/// One tool's definition as the request carries it.
fn definition(tool: Tool) -> Json {
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
            "List the conversations td-agent holds, the most recently active first: each one's id, workspace, state (idle, running, paused, failed), background processes, cost and last activity. The title and the todo item in progress are shown for this conversation only, since other conversations' models wrote them.".to_string(),
            schema(Vec::new(), &[]),
        ),
        Tool::SendMessage => (
            "Send a message to another conversation, by its id from `conversations`. The person approves each message before it is sent, and may refuse it. It is delivered between that conversation's turns, labelled with this conversation as its source, and starts a turn there; any reply comes back to you the same way, later, so do not wait for one. At most 32 KiB, and a conversation holds at most 16 messages undelivered.".to_string(),
            schema(
                vec![
                    ("to", property("string", "The receiving conversation's id.")),
                    ("text", property("string", "The message.")),
                ],
                &["to", "text"],
            ),
        ),
        Tool::ReadFile => (
            format!("Read a text file in the workspace or a shared directory, by its absolute path, as numbered lines: at most {} lines or {} KiB from `offset` (the first line is 1). A view that stops short says so and names the offset to go on from. Every read returns the file's digest, which a later write_file or edit_file of it needs: read a file before changing it. Reading a directory is an error; list one with glob.", files::MAX_LINES, files::MAX_READ_BYTES / 1024),
            schema(
                vec![
                    ("path", property("string", "The file's absolute path.")),
                    ("offset", integer("The first line to show, from 1; 1 when left out.", 1, None)),
                    ("limit", integer("The most lines to show.", 1, Some(files::MAX_LINES))),
                ],
                &["path"],
            ),
        ),
        Tool::WriteFile => (
            "Create a file, or replace one whole, with `content`. Replacing a file needs this conversation to have read it, and the file to be unchanged since. The person approves each write before it is made, and may refuse it.".to_string(),
            schema(
                vec![
                    ("path", property("string", "The file's absolute path.")),
                    ("content", property("string", "The file's whole new text.")),
                ],
                &["path", "content"],
            ),
        ),
        Tool::EditFile => (
            "Replace `old_string` in a file with `new_string`: an exact match, which must be unique unless `replace_all` is true. Give enough surrounding text to make it unique; no match, or several, is an error that says which. The file must have been read by this conversation and be unchanged since. The person approves each edit before it is made, and may refuse it.".to_string(),
            schema(
                vec![
                    ("path", property("string", "The file's absolute path.")),
                    ("old_string", property("string", "The exact text to replace.")),
                    ("new_string", property("string", "What replaces it.")),
                    ("replace_all", property("boolean", "Replace every match rather than one unique match; false when left out.")),
                ],
                &["path", "old_string", "new_string"],
            ),
        ),
        Tool::Glob => (
            format!("List the files whose paths match a glob pattern (`*`, `?`, `[...]`, `**` for any depth, `{{a,b}}`), sorted, at most {}, under `path` or the working directory.", files::MAX_GLOB),
            schema(
                vec![
                    ("pattern", property("string", "The pattern, relative to `path`.")),
                    ("path", property("string", "The absolute directory to search; the working directory when left out.")),
                ],
                &["pattern"],
            ),
        ),
        Tool::Grep => (
            format!("Search files for a POSIX regular expression, as `grep -rn`: each match as path:line:text, at most {} lines. Basic expressions unless `extended`.", shell::MAX_GREP_LINES),
            schema(
                vec![
                    ("pattern", property("string", "The regular expression.")),
                    ("path", property("string", "The absolute file or directory to search; the working directory when left out.")),
                    ("include", property("string", "Only files whose names match this glob.")),
                    ("exclude", property("string", "No files whose names match this glob.")),
                    ("extended", property("boolean", "Extended rather than basic expressions; false when left out.")),
                    ("ignore_case", property("boolean", "Match regardless of case; false when left out.")),
                    ("context", integer("Lines of context around each match.", 0, Some(u64::from(shell::MAX_CONTEXT)))),
                ],
                &["pattern"],
            ),
        ),
        Tool::Sed => (
            "Run a sed script over the named files in place, for a change across many files that edit_file would take many calls to make. The script reads and writes only those files: commands that run a program or touch another file are refused. The person approves it before it runs, and may refuse it.".to_string(),
            schema(
                vec![
                    ("script", property("string", "The sed script, such as s/old/new/g.")),
                    (
                        "paths",
                        Json::Obj(vec![
                            ("type".into(), Json::Str("array".into())),
                            ("items".into(), property("string", "A file's absolute path.")),
                            ("minItems".into(), Json::from(1u64)),
                            ("description".into(), Json::Str("The files to edit.".into())),
                        ]),
                    ),
                    ("extended", property("boolean", "Extended rather than basic expressions; false when left out.")),
                ],
                &["script", "paths"],
            ),
        ),
        Tool::Shell => (
            format!("Run a command with sh -c in the workspace, in a fresh jail of its own: the working directory and shared directories are there, the network and the rest of this machine are not, and nothing it starts outlives the call. The working directory is the workspace's unless `workdir` names another directory in it, and does not persist between calls. Returns the exit status and the output, its middle cut when long. Default timeout {} s, at most {} s. The person approves each command before it runs, and may refuse it.", shell::DEFAULT_TIMEOUT.as_secs(), shell::MAX_TIMEOUT.as_secs()),
            schema(
                vec![
                    ("command", property("string", "The command, as sh -c takes it.")),
                    ("timeout_ms", integer("How long it may run, in milliseconds.", 1, Some(shell::MAX_TIMEOUT.as_millis() as u64))),
                    ("workdir", property("string", "The absolute directory to run in.")),
                ],
                &["command"],
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

/// A conversation's request prefix (DESIGN.md §13): its tools, a
/// workspace's with them when it works in one, and their settings, then
/// the messages every request begins with, `messages` last so a request
/// appends to it.
pub fn prefix(workspace: bool, system: &str) -> String {
    let tools = Tool::all(workspace).into_iter().map(definition).collect();
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

/// The conversation `text` names; a refusal quotes it, cut to 64
/// characters, and adds `hint`.
fn conversation_id(text: &str, member: &str, hint: &str) -> Result<Id, String> {
    Id::parse(text).ok_or_else(|| {
        let quoted: String = text.chars().take(64).collect();
        let cut = if quoted.len() < text.len() { "\u{2026}" } else { "" };
        format!("`{member}` is {quoted:?}{cut}, not a conversation id (32 lowercase hexadecimal digits from `conversations`){hint}")
    })
}

/// `history_search`'s arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Search {
    pub query: String,
    pub conversation: Option<Id>,
    pub kinds: Vec<Searchable>,
    pub limit: usize,
}

/// `history_read`'s arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Read {
    pub conversation: Option<Id>,
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
    Send {
        to: Id,
        text: String,
    },
    /// A workspace tool's, for the tool host; `acts` when the human
    /// decides it first. A write's or an edit's expected digest is the
    /// conversation's to fill.
    Host {
        call: Call,
        acts: bool,
    },
}

/// A boolean member, when present.
fn flag(members: &[(String, Json)], name: &str) -> Result<bool, String> {
    match member(members, name) {
        None => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| format!("`{name}` is not true or false")),
    }
}

/// A workspace tool's call, its arguments checked as far as their shape;
/// what they name is the tool host's to judge, inside the jail.
fn host_call(tool: Tool, value: &Json) -> Result<Call, String> {
    let name = tool.name();
    let owned = |m: &[(String, Json)], key: &str| text(m, key).map(|t| t.map(str::to_string));
    Ok(match tool {
        Tool::ReadFile => {
            let m = members(name, value, &["path", "offset", "limit"])?;
            Call::Read {
                path: required(m, "path")?.to_string(),
                offset: number(m, "offset", 1, u64::MAX)?,
                limit: number(m, "limit", 1, files::MAX_LINES)?,
            }
        }
        Tool::WriteFile => {
            let m = members(name, value, &["path", "content"])?;
            Call::Write {
                path: required(m, "path")?.to_string(),
                content: required(m, "content")?.to_string(),
                expected: None,
            }
        }
        Tool::EditFile => {
            let m = members(
                name,
                value,
                &["path", "old_string", "new_string", "replace_all"],
            )?;
            Call::Edit {
                path: required(m, "path")?.to_string(),
                old: required(m, "old_string")?.to_string(),
                new: required(m, "new_string")?.to_string(),
                all: flag(m, "replace_all")?,
                expected: None,
            }
        }
        Tool::Glob => {
            let m = members(name, value, &["pattern", "path"])?;
            Call::Glob {
                pattern: required(m, "pattern")?.to_string(),
                path: owned(m, "path")?,
            }
        }
        Tool::Grep => {
            let m = members(
                name,
                value,
                &[
                    "pattern",
                    "path",
                    "include",
                    "exclude",
                    "extended",
                    "ignore_case",
                    "context",
                ],
            )?;
            Call::Grep {
                pattern: required(m, "pattern")?.to_string(),
                path: owned(m, "path")?,
                include: owned(m, "include")?,
                exclude: owned(m, "exclude")?,
                extended: flag(m, "extended")?,
                ignore_case: flag(m, "ignore_case")?,
                context: number(m, "context", 0, u64::from(shell::MAX_CONTEXT))?
                    .and_then(|n| u32::try_from(n).ok()),
            }
        }
        Tool::Sed => {
            let m = members(name, value, &["script", "paths", "extended"])?;
            let paths = match member(m, "paths") {
                Some(Json::Arr(paths)) if !paths.is_empty() => paths
                    .iter()
                    .map(|path| {
                        path.as_str()
                            .map(str::to_string)
                            .ok_or_else(|| "`paths` holds something not a string".to_string())
                    })
                    .collect::<Result<_, _>>()?,
                Some(Json::Arr(_)) => return Err("`paths` is empty".into()),
                Some(_) => return Err("`paths` is not a list".into()),
                None => return Err("`paths` is missing".into()),
            };
            Call::Sed {
                script: required(m, "script")?.to_string(),
                paths,
                extended: flag(m, "extended")?,
            }
        }
        Tool::Shell => {
            let m = members(name, value, &["command", "timeout_ms", "workdir"])?;
            let command = required(m, "command")?;
            if command.trim().is_empty() {
                return Err("`command` is empty".into());
            }
            Call::Shell {
                command: command.to_string(),
                timeout_ms: number(m, "timeout_ms", 1, shell::MAX_TIMEOUT.as_millis() as u64)?,
                workdir: owned(m, "workdir")?,
            }
        }
        _ => return Err(format!("{name} is not run by the tool host")),
    })
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

/// A member's value, when present and not null: a model that fills
/// every member of a schema sends null for one it means to leave out.
fn member<'a>(members: &'a [(String, Json)], name: &str) -> Option<&'a Json> {
    members
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v)
        .filter(|v| !v.is_null())
}

/// A string member, when present.
fn text<'a>(members: &'a [(String, Json)], name: &str) -> Result<Option<&'a str>, String> {
    match member(members, name) {
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
    let Some(value) = member(members, name) else {
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

/// The log a history tool reads: another conversation's, or, when the
/// member is left out, null, empty or blank, the caller's own.
fn log_target(members: &[(String, Json)], name: &str) -> Result<Option<Id>, String> {
    text(members, name)?
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| conversation_id(t, name, " (left out, it is this conversation's own log)"))
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
    let items = match member(given, "items") {
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

/// A call to `name` with `arguments` from a conversation: its arguments
/// parsed, or why they cannot be, which the call is answered with and
/// nothing is done.
pub fn parse(name: &str, arguments: &str) -> Result<Args, String> {
    parse_in(false, name, arguments)
}

/// `parse`, for a conversation that works in a workspace when `workspace`.
pub fn parse_in(workspace: bool, name: &str, arguments: &str) -> Result<Args, String> {
    let tool = Tool::find(workspace, name).ok_or_else(|| {
        let names: Vec<&str> = Tool::all(workspace).iter().map(|t| t.name()).collect();
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
            let kinds = match member(m, "kinds") {
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
                conversation: log_target(m, "conversation")?,
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
                conversation: log_target(m, "conversation")?,
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
                to: text(m, "to")?
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| "`to` is missing or empty".to_string())
                    .and_then(|t| conversation_id(t, "to", ""))?,
                text: message_text(m, "text")?,
            }
        }
        Tool::ReadFile
        | Tool::WriteFile
        | Tool::EditFile
        | Tool::Glob
        | Tool::Grep
        | Tool::Sed
        | Tool::Shell => Args::Host {
            call: host_call(tool, &value)?,
            acts: tool.acts(),
        },
    })
}

/// What a crossing check is for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Op {
    Read,
    Message,
}

/// Whether `caller` reading or messaging `target` crosses between
/// workspaces, which the human decides on a card (DESIGN.md §3, §11), or
/// why it is refused outright. Every conversation counts as a workspace
/// of its own, two in one directory included.
pub fn crossing(caller: &Id, target: &Id, op: Op) -> Result<bool, String> {
    if caller != target {
        return Ok(true);
    }
    match op {
        Op::Read => Ok(false),
        Op::Message => Err("a conversation does not send messages to itself".into()),
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
    /// Its workspace as the window's list names it, none outside one.
    pub workspace: Option<String>,
    pub state: String,
    pub cost: u64,
    pub activity: u64,
    pub title: String,
    pub doing: Option<String>,
}

/// The `conversations` result for `caller`: the harness-written fields
/// for every conversation, and the model-written ones only for the
/// caller's own (DESIGN.md §3); `omitted` more were left out.
pub fn listing(caller: &Id, entries: &[Listed], omitted: usize) -> String {
    let mut out = format!("{} conversations", entries.len() + omitted);
    if omitted > 0 {
        out.push_str(&format!(
            ", the {} most recently active shown",
            entries.len()
        ));
    }
    out.push_str(
        "; titles and items in progress are shown for this conversation only, since other conversations' models wrote them:",
    );
    for entry in entries {
        let own = &entry.id == caller;
        out.push_str(&format!(
            "\n{}{} | workspace {} | {} | background 0 | cost {} | active {}",
            entry.id,
            if own { " (this conversation)" } else { "" },
            entry
                .workspace
                .as_deref()
                .map_or("none".to_string(), visible),
            entry.state,
            crate::cost::show(entry.cost),
            crate::history::utc(entry.activity),
        ));
        if own {
            out.push_str(&format!("\n  title: {}", entry.title));
            if let Some(doing) = &entry.doing {
                out.push_str(&format!("\n  in progress: {doing}"));
            }
        }
    }
    out
}

/// The most lines a card shows, and the most bytes of them all, and of
/// one, within what td-ui's dialog takes; and the most lines and bytes
/// one part of it shows, so that no part can push another off the card.
const CARD_LINES: usize = 240;
const CARD_BYTES: usize = 128 * 1024;
const CARD_LINE_BYTES: usize = 2048;
const PART_LINES: usize = 80;
const PART_BYTES: usize = 48 * 1024;

/// A card's lines, as td-ui's dialog takes them: the action's text made
/// visible, cut where the bounds say and the cut said, so the human knows
/// when the card does not show all of it.
#[derive(Default)]
struct Lines {
    lines: Vec<String>,
    bytes: usize,
    /// Lines not shown for want of room on the card.
    unshown: usize,
}

/// `text`'s lines, as a card shows them: split at each newline, a final
/// newline ending the last line rather than beginning another.
fn pieces(text: &str) -> Vec<&str> {
    let mut pieces: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        pieces.pop();
    }
    pieces
}

impl Lines {
    /// One line of td-agent's own.
    fn line(&mut self, line: String) {
        self.push(line);
    }

    /// `text` as one part of the card, at most `most` lines, each after
    /// `mark`, saying how many more it has.
    fn text(&mut self, mark: &str, text: &str, most: usize) {
        let pieces = pieces(text);
        let mut bytes = 0usize;
        for (shown, piece) in pieces.iter().enumerate() {
            if shown >= most.min(PART_LINES * 2) || bytes >= PART_BYTES {
                self.push(format!(
                    "{mark}\u{2026} and {} more lines of it, not shown",
                    pieces.len() - shown
                ));
                return;
            }
            let line = format!("{mark}{}", visible(piece));
            bytes += line.len();
            self.push(line);
        }
    }

    fn push(&mut self, mut line: String) {
        if self.lines.len() >= CARD_LINES || self.bytes >= CARD_BYTES {
            self.unshown += 1;
            return;
        }
        if line.len() > CARD_LINE_BYTES {
            let mut at = CARD_LINE_BYTES;
            while !line.is_char_boundary(at) {
                at -= 1;
            }
            let more = line.len() - at;
            line.truncate(at);
            line.push_str(&format!(
                " \u{2026} ({more} more bytes on this line, not shown)"
            ));
        }
        self.bytes += line.len();
        self.lines.push(line);
    }

    fn done(mut self) -> Vec<String> {
        if self.unshown > 0 {
            self.lines.push(format!(
                "\u{2026} and {} more lines, not shown.",
                self.unshown
            ));
        }
        self.lines
    }
}

/// `line` with what would not show, or would show as something else,
/// named as `<U+XXXX>`: control characters; every whitespace character
/// but a plain space, a tab included, so that nothing that looks like a
/// space can hide where `sh` ends a word; and the invisible and
/// bidirectional characters that could make one command look like
/// another.
pub fn visible(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        let hidden = c.is_control()
            || (c.is_whitespace() && c != ' ')
            || matches!(
                c,
                '\u{ad}'
                    | '\u{34f}'
                    | '\u{61c}'
                    | '\u{115f}'
                    | '\u{1160}'
                    | '\u{17b4}'
                    | '\u{17b5}'
                    | '\u{180b}'..='\u{180f}'
                    | '\u{200b}'..='\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2060}'..='\u{206f}'
                    | '\u{2800}'
                    | '\u{3164}'
                    | '\u{fe00}'..='\u{fe0f}'
                    | '\u{feff}'
                    | '\u{ffa0}'
                    | '\u{fff0}'..='\u{fffb}'
                    | '\u{e0000}'..='\u{e0fff}'
            );
        if hidden {
            out.push_str(&format!("<U+{:04X}>", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    out
}

/// The card that asks the human whether `call` may run (DESIGN.md §11):
/// its title and the exact action, line by line, what it runs before the
/// lists it runs over.
pub fn card(call: &Call) -> (String, Vec<String>) {
    let mut card = Lines::default();
    let title = match call {
        Call::Shell {
            command,
            timeout_ms,
            workdir,
        } => {
            let timeout =
                timeout_ms.map_or(shell::DEFAULT_TIMEOUT.as_secs(), |ms| ms.div_ceil(1000));
            card.line(format!(
                "In {}, for at most {timeout} s, in a jail of its own:",
                workdir
                    .as_deref()
                    .map_or("the workspace".to_string(), visible)
            ));
            card.text("", command, PART_LINES * 2);
            "Run a command"
        }
        Call::Write { path, content, .. } => {
            card.line(format!(
                "{}, made or replaced whole with {} bytes in {} lines:",
                visible(path),
                content.len(),
                pieces(content).len()
            ));
            card.text("", content, PART_LINES * 2);
            "Write a file"
        }
        Call::Edit {
            path,
            old,
            new,
            all,
            ..
        } => {
            card.line(format!(
                "In {}, {} of this text:",
                visible(path),
                if *all { "every match" } else { "the one match" }
            ));
            card.text("- ", old, PART_LINES);
            card.line("replaced with:".into());
            card.text("+ ", new, PART_LINES);
            "Edit a file"
        }
        Call::Sed {
            script,
            paths,
            extended,
        } => {
            card.line(format!(
                "This script, with {} expressions:",
                if *extended { "extended" } else { "basic" }
            ));
            card.text("", script, PART_LINES);
            card.line(format!("over {} files, in place:", paths.len()));
            // Each made visible first, so a newline in one is named
            // rather than listing a file sed does not run over.
            let listed: Vec<String> = paths
                .iter()
                .map(|path| format!("  {}", visible(path)))
                .collect();
            card.text("", &listed.join("\n"), PART_LINES);
            "Run sed over files"
        }
        Call::Read { path, .. } => {
            card.line(visible(path));
            "Read a file"
        }
        Call::Glob { pattern, .. } | Call::Grep { pattern, .. } => {
            card.line(visible(pattern));
            "Search the workspace"
        }
    };
    (title.to_string(), card.done())
}

/// What a crossing would do with the other conversation.
pub enum Reach<'a> {
    Message(&'a str),
    Search(&'a str),
    Read {
        from: u64,
        offset: u64,
        count: usize,
        max_bytes: usize,
    },
}

/// The card that asks the human whether this conversation may reach
/// conversation `target`, which its model titled `title` (DESIGN.md §3,
/// §11): the message whole, or what a search or read asks for and where
/// what it finds goes.
pub fn crossing_card(target: &Id, title: &str, reach: Reach) -> (String, Vec<String>) {
    const DISCLOSES: &str = "What it finds, tool output included, comes into this conversation's context and so to this conversation's model provider.";
    let mut card = Lines::default();
    card.line(format!("Conversation {target}, titled {}", visible(title)));
    let heading = match reach {
        Reach::Message(text) => {
            card.line("gets this message, labelled as from this conversation, not from you, and starting a turn there:".into());
            card.text("", text, PART_LINES * 2);
            "Send a message to another conversation"
        }
        Reach::Search(query) => {
            card.line(format!(
                "has its whole log searched for: {}",
                visible(query)
            ));
            card.line(DISCLOSES.into());
            "Search another conversation's log"
        }
        Reach::Read {
            from,
            offset,
            count,
            max_bytes,
        } => {
            card.line(format!(
                "has its log read: up to {count} events and {max_bytes} bytes from event {from}, {offset} bytes in."
            ));
            card.line(DISCLOSES.into());
            "Read another conversation's log"
        }
    };
    (heading.to_string(), card.done())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;

    fn id(n: u8) -> Id {
        Id::parse(&format!("{n:032x}")).unwrap()
    }

    #[test]
    fn the_prefix_defines_the_tools_before_its_messages() {
        {
            let text = prefix(false, "system text");
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
            assert_eq!(
                names,
                [
                    "todo_write",
                    "history_search",
                    "history_read",
                    "conversations",
                    "send_message"
                ]
            );
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
            assert_eq!(text, prefix(false, "system text"));
        }
    }

    #[test]
    fn a_workspace_adds_its_tools_and_their_calls_go_to_the_host() {
        let names: Vec<&str> = Tool::all(true).iter().map(|t| t.name()).collect();
        assert_eq!(
            names[names.len() - 7..],
            [
                "read_file",
                "write_file",
                "edit_file",
                "glob",
                "grep",
                "sed",
                "shell"
            ]
        );
        // The tool host's tools are offered only in a workspace.
        let outside = parse("shell", r#"{"command":"ls"}"#).unwrap_err();
        assert!(
            outside.starts_with("there is no tool named \"shell\""),
            "{outside}"
        );
        let call = |name: &str, args: &str| parse_in(true, name, args);
        assert_eq!(
            call("shell", r#"{"command":"ls -l","timeout_ms":null}"#).unwrap(),
            Args::Host {
                call: Call::Shell {
                    command: "ls -l".into(),
                    timeout_ms: None,
                    workdir: None
                },
                acts: true
            }
        );
        assert_eq!(
            call("read_file", r#"{"path":"/w/a","offset":3}"#).unwrap(),
            Args::Host {
                call: Call::Read {
                    path: "/w/a".into(),
                    offset: Some(3),
                    limit: None
                },
                acts: false
            }
        );
        assert_eq!(
            call(
                "edit_file",
                r#"{"path":"/w/a","old_string":"x","new_string":"y","replace_all":true}"#
            )
            .unwrap(),
            Args::Host {
                call: Call::Edit {
                    path: "/w/a".into(),
                    old: "x".into(),
                    new: "y".into(),
                    all: true,
                    expected: None
                },
                acts: true
            }
        );
        assert_eq!(
            call("sed", r#"{"script":"s/a/b/","paths":["/w/a","/w/b"]}"#).unwrap(),
            Args::Host {
                call: Call::Sed {
                    script: "s/a/b/".into(),
                    paths: vec!["/w/a".into(), "/w/b".into()],
                    extended: false
                },
                acts: true
            }
        );
        // The model never names the digest a write expects.
        assert!(call(
            "write_file",
            r#"{"path":"/w/a","content":"","expected":"d"}"#
        )
        .is_err());
        for (name, args, why) in [
            ("shell", r#"{"command":"  "}"#, "`command` is empty"),
            (
                "shell",
                r#"{"command":"x","timeout_ms":600001}"#,
                "timeout_ms",
            ),
            ("sed", r#"{"script":"p","paths":[]}"#, "`paths` is empty"),
            ("sed", r#"{"script":"p","paths":[1]}"#, "not a string"),
            ("grep", r#"{"pattern":"x","context":21}"#, "context"),
            ("read_file", r#"{"path":"/w/a","limit":2001}"#, "limit"),
            (
                "edit_file",
                r#"{"path":"/a","old_string":"x","new_string":"y","replace_all":"yes"}"#,
                "true or false",
            ),
        ] {
            let e = call(name, args).unwrap_err();
            assert!(e.contains(why), "{name} {args}: {e}");
        }
        assert!(
            Tool::Shell.acts()
                && Tool::Sed.acts()
                && Tool::WriteFile.acts()
                && Tool::EditFile.acts()
        );
        assert!(!Tool::ReadFile.acts() && !Tool::Glob.acts() && !Tool::Grep.acts());
    }

    #[test]
    fn a_card_shows_the_action_whole_or_says_what_it_leaves_out() {
        let (title, lines) = card(&Call::Shell {
            command: "rm -rf build\n\tmake\u{202e}x\u{200b}\u{7}".into(),
            timeout_ms: Some(1500),
            workdir: None,
        });
        assert_eq!(title, "Run a command");
        assert_eq!(
            lines,
            [
                "In the workspace, for at most 2 s, in a jail of its own:",
                "rm -rf build",
                "<U+0009>make<U+202E>x<U+200B><U+0007>",
            ]
        );
        let (_, lines) = card(&Call::Edit {
            path: "/w/a".into(),
            old: "one\ntwo".into(),
            new: "three".into(),
            all: false,
            expected: None,
        });
        assert_eq!(
            lines,
            [
                "In /w/a, the one match of this text:",
                "- one",
                "- two",
                "replaced with:",
                "+ three"
            ]
        );
        // Past its bounds a card says how much it leaves out, and every
        // line fits td-ui's dialog.
        let long = format!("{}\n", "x".repeat(5000)).repeat(300);
        let (title, lines) = card(&Call::Write {
            path: "/w/big".into(),
            content: long.clone(),
            expected: None,
        });
        assert_eq!(title, "Write a file");
        assert!(lines.len() <= CARD_LINES + 1);
        assert!(lines.len() <= td_ui::confirmations::DETAILS);
        assert!(lines
            .iter()
            .all(|l| l.len() <= td_ui::confirmations::DETAIL_BYTES));
        assert!(lines.iter().map(String::len).sum::<usize>() <= CARD_BYTES + 8192);
        assert!(lines[0].contains("in 300 lines:"), "{}", lines[0]);
        assert!(
            lines[1].ends_with("more bytes on this line, not shown)"),
            "{}",
            lines[1]
        );
        assert!(
            lines
                .last()
                .unwrap()
                .contains("more lines of it, not shown"),
            "{lines:?}"
        );
        assert!(lines.iter().all(|l| !l.chars().any(char::is_control)));
        // A sed script is shown before its files, however many, and an
        // edit's replacement however long the text it replaces.
        let paths: Vec<String> = (0..500).map(|n| format!("/w/{n}")).collect();
        let (_, lines) = card(&Call::Sed {
            script: "s/a/b/".into(),
            paths,
            extended: false,
        });
        assert_eq!(lines[1], "s/a/b/");
        let (_, odd) = card(&Call::Sed {
            script: "p".into(),
            paths: vec!["/w/a\n  /w/b".into()],
            extended: false,
        });
        assert_eq!(odd[3], "  /w/a<U+000A>  /w/b");
        assert_eq!(odd.len(), 4);
        assert!(
            lines.iter().any(|l| l.contains("420 more lines of it")),
            "{lines:?}"
        );
        let (_, lines) = card(&Call::Edit {
            path: "/w/a".into(),
            old: "x\n".repeat(1000),
            new: "the new text".into(),
            all: true,
            expected: None,
        });
        assert!(lines.contains(&"+ the new text".to_string()), "{lines:?}");
    }

    /// Nothing that looks like a space, or like nothing, passes for one:
    /// `make test<NBSP># ; rm -rf x` is one word, a separator and `rm`.
    #[test]
    fn a_card_names_what_could_hide_a_word_or_a_command() {
        for c in [
            '\u{a0}', '\u{1680}', '\u{2000}', '\u{200a}', '\u{202f}', '\u{205f}', '\u{3000}',
            '\u{2800}', '\u{85}', '\u{2028}', '\u{2029}', '\t', '\u{b}', '\u{c}', '\r',
        ] {
            let shown = visible(&format!("a{c}b"));
            assert_eq!(shown, format!("a<U+{:04X}>b", u32::from(c)));
        }
        assert_eq!(
            visible("make test\u{a0}# ; rm -rf x"),
            "make test<U+00A0># ; rm -rf x"
        );
        assert_eq!(visible("plain text, é and 日本"), "plain text, é and 日本");
        // A file ending in a newline has as many lines as its card shows.
        let (_, lines) = card(&Call::Write {
            path: "/w/a".into(),
            content: "one\ntwo\n".into(),
            expected: None,
        });
        assert_eq!(
            lines,
            [
                "/w/a, made or replaced whole with 8 bytes in 2 lines:",
                "one",
                "two"
            ]
        );
    }

    #[test]
    fn a_todo_list_is_held_to_its_bounds() {
        let item = |content: &str, status: &str| {
            format!(r#"{{"content":"{content}","status":"{status}"}}"#)
        };
        let call = |items: &[String]| {
            parse(
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
            let e = parse("todo_write", arguments).unwrap_err();
            assert!(e.contains(said), "{arguments}: {e}");
        }
        let big = format!(r#"{{"items":[],"x":"{}"}}"#, "a".repeat(MAX_ARGUMENTS));
        assert!(parse("todo_write", &big).unwrap_err().contains("bound"));
        let unknown = parse("read_file", "{}").unwrap_err();
        assert!(unknown.contains("no tool named \"read_file\""), "{unknown}");
        // There is no report, nor any orchestrator to send one to.
        let report = parse("report", r#"{"status":"done","summary":"x"}"#).unwrap_err();
        assert!(report.contains("no tool named \"report\""), "{report}");
        // No arguments at all is an empty object.
        assert_eq!(parse("conversations", "").unwrap(), Args::Conversations);
        assert!(parse("conversations", r#"{"all":true}"#).is_err());
        for (arguments, said) in [
            (r#"{"query":"  "}"#, "no terms"),
            (r#"{"query":"a","limit":0}"#, "outside 1 to 100"),
            (r#"{"query":"a","limit":101}"#, "outside 1 to 100"),
            (r#"{"query":"a","limit":"5"}"#, "not a whole number"),
            (r#"{"query":"a","kinds":["mail"]}"#, "not a kind"),
            (
                r#"{"query":"a","conversation":"bob"}"#,
                "`conversation` is \"bob\", not a conversation id",
            ),
        ] {
            let e = parse("history_search", arguments).unwrap_err();
            assert!(e.contains(said), "{arguments}: {e}");
        }
        for (arguments, said) in [
            (r#"{}"#, "`from` is missing"),
            (r#"{"from":0}"#, "outside 1"),
            (r#"{"from":1,"count":101}"#, "outside 1 to 100"),
            (r#"{"from":1,"max_bytes":262145}"#, "outside 1 to 262144"),
        ] {
            let e = parse("history_read", arguments).unwrap_err();
            assert!(e.contains(said), "{arguments}: {e}");
        }
        for (arguments, said) in [
            (
                r#"{"to":"00000000000000000000000000000001"}"#,
                "`text` is missing",
            ),
            (
                r#"{"to":"00000000000000000000000000000001","text":" "}"#,
                "empty",
            ),
            (
                r#"{"to":"orchestrator","text":"hi"}"#,
                "`to` is \"orchestrator\", not a conversation id",
            ),
            (r#"{"text":"hi"}"#, "`to` is missing"),
            // A blank or null receiver is never a default one.
            (r#"{"to":"","text":"hi"}"#, "`to` is missing or empty"),
            (r#"{"to":"  ","text":"hi"}"#, "`to` is missing or empty"),
            (r#"{"to":null,"text":"hi"}"#, "`to` is missing or empty"),
            (
                r#"{"to":"bob","text":"hi"}"#,
                "`to` is \"bob\", not a conversation id",
            ),
        ] {
            let e = parse("send_message", arguments).unwrap_err();
            assert!(e.contains(said), "{arguments}: {e}");
        }
        let long = format!(
            r#"{{"to":"00000000000000000000000000000001","text":"{}"}}"#,
            "x".repeat(MAX_MESSAGE + 1)
        );
        assert!(parse("send_message", &long)
            .unwrap_err()
            .contains("at most 32768"));
    }

    #[test]
    fn arguments_parse_with_their_defaults() {
        assert_eq!(
            parse("history_search", r#"{"query":"build failed"}"#).unwrap(),
            Args::Search(Search {
                query: "build failed".into(),
                conversation: None,
                kinds: Vec::new(),
                limit: SEARCH_LIMIT,
            })
        );
        // The diagnostics of a live session: a model filling every member
        // sends an empty or null one for its own log.
        for arguments in [
            r#"{"query":"build failed","conversation":"","kinds":null,"limit":null}"#,
            r#"{"query":"build failed","conversation":null}"#,
        ] {
            assert_eq!(
                parse("history_search", arguments).unwrap(),
                Args::Search(Search {
                    query: "build failed".into(),
                    conversation: None,
                    kinds: Vec::new(),
                    limit: SEARCH_LIMIT,
                }),
                "{arguments}"
            );
        }
        for (arguments, conversation) in [
            (r#"{"from":1,"conversation":"   "}"#, None),
            (r#"{"from":1,"conversation":null}"#, None),
            (
                r#"{"from":1,"conversation":" 00000000000000000000000000000007 "}"#,
                Some(id(7)),
            ),
        ] {
            let Args::Read(read) = parse("history_read", arguments).unwrap() else {
                panic!("{arguments}")
            };
            assert_eq!(read.conversation, conversation, "{arguments}");
        }
        // A refusal says what leaving it out means, and quotes a long
        // value cut.
        let long = format!(r#"{{"query":"a","conversation":"{}"}}"#, "x".repeat(300));
        let e = parse("history_search", &long).unwrap_err();
        assert!(e.contains(&format!("{:?}\u{2026}", "x".repeat(64))), "{e}");
        assert!(
            e.contains("left out, it is this conversation's own log"),
            "{e}"
        );
        // A null required member is missing.
        let e = parse("history_search", r#"{"query":null}"#).unwrap_err();
        assert!(e.contains("`query` is missing"), "{e}");
        let other = format!("{}", id(7));
        assert_eq!(
            parse(
                "history_read",
                &format!(
                    r#"{{"conversation":"{other}","from":3,"offset":10,"count":5,"max_bytes":100}}"#
                )
            )
            .unwrap(),
            Args::Read(Read {
                conversation: Some(id(7)),
                from: 3,
                offset: 10,
                count: 5,
                max_bytes: 100,
            })
        );
        assert_eq!(
            parse("history_read", r#"{"from":1}"#).unwrap(),
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
                "send_message",
                &format!(r#"{{"to":"{other}","text":"go"}}"#)
            )
            .unwrap(),
            Args::Send {
                to: id(7),
                text: "go".into()
            }
        );
    }

    #[test]
    fn reaching_another_conversation_is_a_crossing_and_its_own_log_is_not() {
        let (a, b) = (id(2), id(3));
        assert_eq!(crossing(&a, &a, Op::Read), Ok(false));
        assert_eq!(crossing(&a, &b, Op::Read), Ok(true));
        assert_eq!(crossing(&a, &b, Op::Message), Ok(true));
        assert_eq!(crossing(&b, &a, Op::Message), Ok(true));
        assert!(crossing(&a, &a, Op::Message)
            .unwrap_err()
            .contains("itself"));
    }

    /// A crossing's card names the other conversation, its title made
    /// visible, and shows a message whole or says where a read's findings
    /// go.
    #[test]
    fn a_crossing_card_shows_what_crosses() {
        let (title, lines) =
            crossing_card(&id(3), "Fix\u{202e}the build", Reach::Message("one\ntwo"));
        assert_eq!(title, "Send a message to another conversation");
        assert_eq!(
            lines[0],
            format!("Conversation {}, titled Fix<U+202E>the build", id(3))
        );
        assert_eq!(lines[2..], ["one", "two"]);
        let (title, lines) = crossing_card(&id(3), "t", Reach::Search("build\tfailed"));
        assert_eq!(title, "Search another conversation's log");
        assert!(lines[1].ends_with("build<U+0009>failed"), "{lines:?}");
        assert!(lines[2].contains("model provider"), "{lines:?}");
        let reach = Reach::Read {
            from: 4,
            offset: 7,
            count: 20,
            max_bytes: 100,
        };
        let (title, lines) = crossing_card(&id(3), "t", reach);
        assert_eq!(title, "Read another conversation's log");
        assert!(
            lines[1].contains("up to 20 events and 100 bytes from event 4, 7 bytes in"),
            "{lines:?}"
        );
    }

    #[test]
    fn the_listing_shows_model_written_fields_to_their_own_alone() {
        let entry = |n: u8, workspace: Option<&str>| Listed {
            id: id(n),
            workspace: workspace.map(str::to_string),
            state: "idle".into(),
            cost: 0,
            activity: 0,
            title: format!("title {n}"),
            doing: Some(format!("doing {n}")),
        };
        let entries = [
            entry(1, None),
            entry(2, Some("scratch")),
            entry(3, Some("/home/u/notes\n")),
        ];
        let mine = listing(&id(2), &entries, 0);
        assert!(
            mine.contains("title 2") && mine.contains("doing 2"),
            "{mine}"
        );
        for hidden in ["title 1", "doing 1", "title 3", "doing 3"] {
            assert!(!mine.contains(hidden), "{hidden} in {mine}");
        }
        assert!(
            mine.contains(&format!("{} | workspace none", id(1))),
            "{mine}"
        );
        assert!(
            mine.contains(&format!(
                "{} (this conversation) | workspace scratch",
                id(2)
            )),
            "{mine}"
        );
        // A workspace's name is the human's, and shown made visible.
        assert!(
            mine.contains(&format!("{} | workspace /home/u/notes<U+000A> |", id(3))),
            "{mine}"
        );
        let all = listing(&id(1), &entries, 2);
        assert!(all.contains("title 1"), "{all}");
        assert!(!all.contains("title 2"), "{all}");
        assert!(
            all.starts_with("5 conversations, the 3 most recently active shown; titles"),
            "{all}"
        );
    }
}
