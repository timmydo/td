//! The MCP client's side of the Model Context Protocol (DESIGN.md §12,
//! MCP servers): newline-delimited JSON-RPC 2.0 over a server's standard
//! input and output, with no client capability, so only `initialize`,
//! `tools/list` and `tools/call` are asked. It speaks over a `Transport`,
//! which a server's `serve` call is; everything a server says is the
//! jail's, read within bounds and never taken as authority.

use std::collections::{BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use td_json::Json;

/// The protocol version asked for, and those whose replies are taken: each
/// carries the methods td-agent uses unchanged.
pub const PROTOCOL: &str = "2025-06-18";
pub const PROTOCOLS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
/// The longest line a server may write.
pub const MAX_LINE: usize = 1024 * 1024;
/// How long `initialize` and the listing have together.
pub const START_TIME: Duration = Duration::from_secs(30);
/// The most pages of `tools/list` followed, and the most tools kept from
/// them, the rest left as pages past them are.
pub const MAX_PAGES: usize = 8;
pub const MAX_LISTED: usize = 1024;
/// The most reasons a server's tools were left out that are said one by
/// one; past them, how many more.
pub const MAX_LEFT: usize = 32;
/// The most requests td-agent stopped waiting for that a server may leave
/// unanswered before it is ended.
pub const MAX_ABANDONED: usize = 8;
/// A tool's own name's bytes: `mcp__` and a server's name of at most 16
/// and `__` leave 41 of a tool name's 64.
pub const MAX_TOOL_NAME: usize = 41;
/// A tool's description after it is made visible, and its schema as
/// encoded.
pub const MAX_DESCRIPTION: usize = 2048;
pub const MAX_SCHEMA: usize = 16 * 1024;
/// The deepest a schema nests, well within td-json's depth beside the
/// definition, the log event and the window's frame that carry it.
pub const MAX_SCHEMA_DEPTH: usize = 64;
/// Every server's tools together, as the prefix carries them.
pub const MAX_TOOLS: usize = 128;
pub const MAX_TOOLS_BYTES: usize = 128 * 1024;
/// The most characters of a server's own text, an error's message say,
/// quoted in what td-agent says, as a repository's failure is (§3).
const MAX_QUOTED: usize = 1000;
/// How often a wait looks at whether it should stop.
const SLICE: Duration = Duration::from_millis(100);

/// Where a server's bytes go and come from.
pub trait Transport {
    /// Sends `bytes` whole, or says why it could not, never waiting on
    /// the server: what cannot go yet is held, within a bound of the
    /// transport's past which it errs, and sent as `receive` is called,
    /// so a server that stops reading holds up no deadline or stop.
    fn send(&mut self, bytes: &[u8]) -> Result<(), String>;
    /// The next piece of the server's output, waiting until `deadline`;
    /// none only once `deadline` has come, which the reader's wait relies
    /// on so as not to spin. An error means the server is gone.
    fn receive(&mut self, deadline: Instant) -> Result<Option<Vec<u8>>, String>;
}

/// Why a request did not get its answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Failure {
    /// The server broke the protocol, failed, or is gone: it is ended.
    Broke(String),
    /// The wait was stopped, an interrupt say; the request was cancelled.
    Stopped,
    /// The request's time ran out; it was cancelled.
    TimedOut,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Broke(why) => write!(f, "{why}"),
            Self::Stopped => write!(f, "it was cancelled, and its effect is unknown"),
            Self::TimedOut => write!(f, "it timed out, and its effect is unknown"),
        }
    }
}

/// `text` made visible line by line, as a card shows text (§11), each
/// line's ends kept (a carriage return before one dropped), then cut to
/// at most `max` bytes between characters, never within a name it made.
/// Only as much of `text` as could show is made visible.
pub fn quoted(text: &str, max: usize) -> String {
    let shown: Vec<String> = cut(text, max)
        .split('\n')
        .map(|line| shown(line.strip_suffix('\r').unwrap_or(line)))
        .collect();
    whole_names(cut(&shown.join("\n"), max))
}

/// `text` cut to at most `max` bytes, between characters.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default().to_string()
}

/// `text` made visible, a `<` that begins a name in the text itself
/// named too, as a card names it, so every `<U+` in what td-agent shows
/// is a name td-agent made.
fn shown(text: &str) -> String {
    crate::tools::visible(&text.replace("<U+", "<U+003C>U+"))
}

/// `shown` without a `<U+XXXX>` the cut left unfinished at its end.
fn whole_names(mut shown: String) -> String {
    if let Some(at) = shown.rfind("<U+") {
        if !shown.get(at..).unwrap_or_default().contains('>') {
            shown.truncate(at);
        }
    }
    shown
}

/// One line of a server's text, made visible and cut to `MAX_QUOTED`
/// characters, never within a name it made.
fn one_line(text: &str) -> String {
    let source: String = text.chars().take(MAX_QUOTED).collect();
    let shown: String = shown(&source).chars().take(MAX_QUOTED).collect();
    whole_names(shown)
}

/// A server's output cut into lines, each at most `MAX_LINE`.
#[derive(Default)]
pub struct Lines {
    pending: Vec<u8>,
}

impl Lines {
    /// The whole lines `bytes` completes, a trailing carriage return
    /// dropped and empty lines skipped, into `lines`; an error once a
    /// line, its carriage return not counted, runs past `MAX_LINE`.
    pub fn push(&mut self, bytes: &[u8], lines: &mut VecDeque<Vec<u8>>) -> Result<(), String> {
        let past = || format!("the server wrote a line past {MAX_LINE} bytes");
        for &byte in bytes {
            if byte == b'\n' {
                let mut line = std::mem::take(&mut self.pending);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.len() > MAX_LINE {
                    return Err(past());
                }
                if !line.is_empty() {
                    lines.push_back(line);
                }
            } else if self.pending.len() > MAX_LINE {
                return Err(past());
            } else {
                self.pending.push(byte);
            }
        }
        Ok(())
    }
}

/// A JSON-RPC message from a server.
#[derive(Clone, Debug, PartialEq)]
pub enum Incoming {
    /// An answer to request `id` of td-agent's: its result or its error.
    /// One whose id is not a number td-agent could have given has none.
    Response {
        id: Option<u64>,
        outcome: Result<Json, RpcError>,
    },
    /// A request the server asks, which is answered.
    Request {
        id: Json,
        method: String,
    },
    Notification {
        method: String,
    },
}

/// A JSON-RPC error's code and message, the message as the server wrote
/// it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the server answered error {}: {}",
            self.code,
            one_line(&self.message)
        )
    }
}

/// `line` read as one JSON-RPC 2.0 message, or why it is not one: a
/// batch, or anything but an object of version 2.0 that is a request, a
/// notification or a response, is refused.
pub fn parse(line: &[u8]) -> Result<Incoming, String> {
    // The parser's words may quote the server's text.
    let value = td_json::parse_slice(line).map_err(|e| {
        format!(
            "the server wrote what is not JSON: {}",
            one_line(&e.to_string())
        )
    })?;
    if value.as_arr().is_some() {
        return Err("the server wrote a batch, which td-agent does not take".into());
    }
    if value.get("jsonrpc").and_then(Json::as_str) != Some("2.0") {
        return Err("the server wrote a message that is not JSON-RPC 2.0".into());
    }
    if let Some(method) = value.get("method") {
        let method = method
            .as_str()
            .ok_or("the server wrote a method that is not a string")?
            .to_string();
        return Ok(match value.get("id") {
            None => Incoming::Notification { method },
            Some(id @ (Json::Num(_) | Json::Str(_))) => Incoming::Request {
                id: id.clone(),
                method,
            },
            Some(_) => return Err("the server's request has an id of no kind".into()),
        });
    }
    let id = value.get("id").and_then(Json::as_u64);
    let outcome = match (value.get("result"), value.get("error")) {
        (Some(result), None) => Ok(result.clone()),
        (None, Some(error)) => Err(RpcError {
            code: error
                .get("code")
                .and_then(|code| match code {
                    Json::Num(n) => n.parse::<i64>().ok(),
                    _ => None,
                })
                .ok_or("the server's error has no code")?,
            message: error
                .get("message")
                .and_then(Json::as_str)
                .ok_or("the server's error has no message")?
                .to_string(),
        }),
        (Some(_), Some(_)) => {
            return Err("the server answered with both a result and an error".into())
        }
        (None, None) => {
            return Err("the server wrote a message that is neither asked nor answered".into())
        }
    };
    // The server could not read what td-agent sent, nor so say which.
    if let (Some(Json::Null), Err(error)) = (value.get("id"), &outcome) {
        return Err(format!("the server could not read a request: {error}"));
    }
    Ok(Incoming::Response { id, outcome })
}

/// A tool as the prefix carries it.
#[derive(Clone, Debug, PartialEq)]
pub struct Offered {
    /// Its name in the prefix, `mcp__SERVER__TOOL`.
    pub name: String,
    /// Its name as the server knows it.
    pub tool: String,
    /// Its definition, exactly as the request's `tools` carries it.
    pub definition: Json,
}

impl Offered {
    /// Its bytes in the prefix.
    pub fn size(&self) -> usize {
        self.definition.to_string().len()
    }
}

/// What starting a server came to: its protocol version, its tools as
/// listed, and what lay past the pages followed.
#[derive(Clone, Debug, PartialEq)]
pub struct Started {
    pub protocol: String,
    pub tools: Vec<Json>,
    pub more: bool,
}

/// One server, spoken to over `T`.
pub struct Server<T: Transport> {
    transport: T,
    lines: Lines,
    /// Lines read and not yet handled, which a reply's wait leaves.
    queued: VecDeque<Vec<u8>>,
    next: u64,
    /// Requests td-agent stopped waiting for, whose replies are dropped.
    abandoned: BTreeSet<u64>,
    /// The server said its tools changed.
    pub list_changed: bool,
}

impl<T: Transport> Server<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            lines: Lines::default(),
            queued: VecDeque::new(),
            next: 1,
            abandoned: BTreeSet::new(),
            list_changed: false,
        }
    }

    /// Its transport, to end it or read what else it holds.
    pub fn transport(&mut self) -> &mut T {
        &mut self.transport
    }

    fn send(&mut self, message: Json) -> Result<(), Failure> {
        let mut bytes = message.to_string().into_bytes();
        bytes.push(b'\n');
        self.transport.send(&bytes).map_err(Failure::Broke)
    }

    fn request(&mut self, method: &str, params: Json) -> Result<u64, Failure> {
        let id = self.next;
        self.next = self.next.saturating_add(1);
        self.send(Json::Obj(vec![
            ("jsonrpc".into(), Json::Str("2.0".into())),
            ("id".into(), Json::from(id)),
            ("method".into(), Json::Str(method.into())),
            ("params".into(), params),
        ]))?;
        Ok(id)
    }

    fn notify(&mut self, method: &str, params: Json) -> Result<(), Failure> {
        self.send(Json::Obj(vec![
            ("jsonrpc".into(), Json::Str("2.0".into())),
            ("method".into(), Json::Str(method.into())),
            ("params".into(), params),
        ]))
    }

    /// Handles one message, answering a request; the outcome when it is
    /// the answer to `awaited`.
    fn handle(
        &mut self,
        message: Incoming,
        awaited: Option<u64>,
    ) -> Result<Option<Result<Json, RpcError>>, Failure> {
        match message {
            Incoming::Response { id, outcome } => {
                if id.is_some() && id == awaited {
                    return Ok(Some(outcome));
                }
                if let Some(id) = id {
                    self.abandoned.remove(&id);
                }
                // An answer to nothing asked, or to what was abandoned, is
                // dropped.
                Ok(None)
            }
            Incoming::Request { id, method } => {
                let answer = if method == "ping" {
                    ("result".to_string(), Json::Obj(Vec::new()))
                } else {
                    (
                        "error".to_string(),
                        Json::Obj(vec![
                            ("code".into(), Json::Num("-32601".into())),
                            ("message".into(), Json::Str("Method not found".into())),
                        ]),
                    )
                };
                self.send(Json::Obj(vec![
                    ("jsonrpc".into(), Json::Str("2.0".into())),
                    ("id".into(), id),
                    answer,
                ]))?;
                Ok(None)
            }
            Incoming::Notification { method } => {
                if method == "notifications/tools/list_changed" {
                    self.list_changed = true;
                }
                Ok(None)
            }
        }
    }

    /// Reads what the server says until `deadline` says it is time,
    /// answering its requests; the outcome of `awaited` when it comes.
    /// `stop` and `deadline` are asked between pieces, so a deadline may
    /// move while its clock is stopped.
    fn read(
        &mut self,
        awaited: Option<u64>,
        deadline: &mut dyn FnMut() -> Instant,
        stop: &mut dyn FnMut() -> bool,
    ) -> Result<Option<Result<Json, RpcError>>, Failure> {
        loop {
            // The lines a piece completed past an answer, first.
            while let Some(line) = self.queued.pop_front() {
                let message = parse(&line).map_err(Failure::Broke)?;
                if let Some(outcome) = self.handle(message, awaited)? {
                    return Ok(Some(outcome));
                }
            }
            if awaited.is_some() && stop() {
                return Err(Failure::Stopped);
            }
            let now = Instant::now();
            let deadline = deadline();
            if now >= deadline {
                return Ok(None);
            }
            let slice = deadline.min(now + SLICE);
            let Some(piece) = self.transport.receive(slice).map_err(Failure::Broke)? else {
                continue;
            };
            self.lines
                .push(&piece, &mut self.queued)
                .map_err(Failure::Broke)?;
        }
    }

    /// Asks `method` and waits for its answer until `deadline`; a wait
    /// stopped or out of time cancels the request.
    fn ask(
        &mut self,
        method: &str,
        params: Json,
        deadline: &mut dyn FnMut() -> Instant,
        cancellable: bool,
        stop: &mut dyn FnMut() -> bool,
    ) -> Result<Result<Json, RpcError>, Failure> {
        let id = self.request(method, params)?;
        let ended = match self.read(Some(id), deadline, stop) {
            Ok(Some(outcome)) => return Ok(outcome),
            Ok(None) => Failure::TimedOut,
            Err(failure) => failure,
        };
        if let Failure::Broke(_) = ended {
            return Err(ended);
        }
        if !cancellable {
            return Err(Failure::Broke(match ended {
                Failure::Stopped => format!("it was ended while td-agent waited for `{method}`"),
                _ => format!("the server did not answer `{method}` in time"),
            }));
        }
        let reason = match ended {
            Failure::Stopped => "interrupted",
            _ => "timed out",
        };
        self.notify(
            "notifications/cancelled",
            Json::Obj(vec![
                ("requestId".into(), Json::from(id)),
                ("reason".into(), Json::Str(reason.into())),
            ]),
        )?;
        self.abandoned.insert(id);
        if self.abandoned.len() > MAX_ABANDONED {
            return Err(Failure::Broke(format!(
                "the server left more than {MAX_ABANDONED} cancelled requests unanswered"
            )));
        }
        Err(ended)
    }

    /// `initialize`, `notifications/initialized` and `tools/list` with
    /// its pages, all until `deadline`, which the caller gives
    /// `START_TIME` but for while its clock is stopped; any failure here,
    /// `stop` included, ends the server, since `initialize` cannot be
    /// cancelled. `stop` is the server's own end, never a turn's
    /// interrupt, which ends only the turn's wait for it (§12, Lifetime).
    pub fn start(
        &mut self,
        deadline: &mut dyn FnMut() -> Instant,
        stop: &mut dyn FnMut() -> bool,
    ) -> Result<Started, Failure> {
        let params = Json::Obj(vec![
            ("protocolVersion".into(), Json::Str(PROTOCOL.into())),
            ("capabilities".into(), Json::Obj(Vec::new())),
            (
                "clientInfo".into(),
                Json::Obj(vec![
                    ("name".into(), Json::Str("td-agent".into())),
                    (
                        "version".into(),
                        Json::Str(env!("CARGO_PKG_VERSION").into()),
                    ),
                ]),
            ),
        ]);
        let reply = self
            .ask("initialize", params, deadline, false, stop)?
            .map_err(|e| Failure::Broke(format!("`initialize`: {e}")))?;
        let protocol = reply
            .get("protocolVersion")
            .and_then(Json::as_str)
            .ok_or_else(|| Failure::Broke("`initialize` named no protocol version".into()))?;
        if !PROTOCOLS.contains(&protocol) {
            return Err(Failure::Broke(format!(
                "the server speaks protocol {}, which td-agent does not",
                one_line(protocol)
            )));
        }
        let protocol = protocol.to_string();
        self.notify("notifications/initialized", Json::Obj(Vec::new()))?;
        let (tools, more) = self.list(deadline, stop)?;
        Ok(Started {
            protocol,
            tools,
            more,
        })
    }

    /// `tools/list` with its pages until `deadline`: the tools, at most
    /// `MAX_LISTED`, and whether what lay past `MAX_PAGES` or them was
    /// left. As `start`, any failure ends the server.
    pub fn list(
        &mut self,
        deadline: &mut dyn FnMut() -> Instant,
        stop: &mut dyn FnMut() -> bool,
    ) -> Result<(Vec<Json>, bool), Failure> {
        self.list_changed = false;
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                None => Json::Obj(Vec::new()),
                Some(cursor) => Json::Obj(vec![("cursor".into(), Json::Str(cursor.clone()))]),
            };
            let page = self
                .ask("tools/list", params, deadline, false, stop)?
                .map_err(|e| Failure::Broke(format!("`tools/list`: {e}")))?;
            let listed = page
                .get("tools")
                .and_then(Json::as_arr)
                .ok_or_else(|| Failure::Broke("`tools/list` gave no list of tools".into()))?;
            let room = MAX_LISTED.saturating_sub(tools.len());
            tools.extend(listed.iter().take(room).cloned());
            if listed.len() > room {
                return Ok((tools, true));
            }
            cursor = match page.get("nextCursor") {
                // An empty cursor, as some servers end with, ends it too.
                None | Some(Json::Null) => return Ok((tools, false)),
                Some(Json::Str(next)) if next.is_empty() => return Ok((tools, false)),
                Some(Json::Str(next)) => Some(next.clone()),
                Some(_) => {
                    return Err(Failure::Broke(
                        "`tools/list` gave a cursor that is not a string".into(),
                    ))
                }
            };
        }
        Ok((tools, true))
    }

    /// Calls `tool` with `arguments` until `timeout`, `stop` asked as it
    /// waits: the result, or the server's error.
    pub fn call(
        &mut self,
        tool: &str,
        arguments: Json,
        timeout: Duration,
        stop: &mut dyn FnMut() -> bool,
    ) -> Result<Result<Json, RpcError>, Failure> {
        let params = Json::Obj(vec![
            ("name".into(), Json::Str(tool.into())),
            ("arguments".into(), arguments),
        ]);
        let end = Instant::now() + timeout;
        let answer = self.ask("tools/call", params, &mut || end, true, stop)?;
        if let Ok(result) = &answer {
            if !matches!(result, Json::Obj(_)) {
                return Err(Failure::Broke(
                    "`tools/call` gave a result that is not an object".into(),
                ));
            }
        }
        Ok(answer)
    }

    /// Reads what the server says for `wait`, answering its requests and
    /// taking its notifications, between calls.
    pub fn pump(&mut self, wait: Duration) -> Result<(), Failure> {
        let end = Instant::now() + wait;
        self.read(None, &mut || end, &mut || false).map(drop)
    }
}

/// What was left out of a server's tools and why: the first
/// `MAX_LEFT` reasons, then only how many more.
#[derive(Default)]
struct Left {
    said: Vec<String>,
    more: usize,
}

impl Left {
    fn push(&mut self, why: String) {
        if self.said.len() < MAX_LEFT {
            self.said.push(why);
        } else {
            self.more += 1;
        }
    }

    fn done(mut self) -> Vec<String> {
        if self.more > 0 {
            self.said.push(format!("and {} more", self.more));
        }
        self.said
    }
}

/// Whether `name` can be a tool's own name in `mcp__SERVER__TOOL`: 1 to
/// `MAX_TOOL_NAME` bytes of ASCII letters, digits, `_` and `-`.
pub fn tool_name_fits(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_TOOL_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `server`'s tools as the prefix offers them, kept to `keep` when it
/// names any: each checked and bounded, at most `MAX_TOOLS`, and what was
/// left out and why. A name listed again repeats the first listing,
/// whether or not that one was offered. `server` is a template's
/// server's name, checked as the configuration is read.
pub fn offer(server: &str, tools: &[Json], keep: Option<&[String]>) -> (Vec<Offered>, Vec<String>) {
    let mut offered: Vec<Offered> = Vec::new();
    let mut left = Left::default();
    let mut listed: BTreeSet<&str> = BTreeSet::new();
    for tool in tools {
        let Some(name) = tool.get("name").and_then(Json::as_str) else {
            // `keep` names none of them.
            if keep.is_none() {
                left.push("a tool with no name".to_string());
            }
            continue;
        };
        let first = listed.insert(name);
        // What the template does not keep is not checked or said.
        if keep.is_some_and(|keep| !keep.iter().any(|k| k == name)) {
            continue;
        }
        let shown = one_line(name);
        if !first {
            left.push(format!("{shown}: listed twice"));
            continue;
        }
        if !tool_name_fits(name) {
            left.push(format!(
                "{shown}: a name is 1 to {MAX_TOOL_NAME} bytes of ASCII letters, digits, `_` and `-`"
            ));
            continue;
        }
        let Some(Json::Obj(members)) = tool.get("inputSchema") else {
            left.push(format!("{shown}: its input schema is not an object"));
            continue;
        };
        if tool
            .get("inputSchema")
            .and_then(|s| s.get("type"))
            .and_then(Json::as_str)
            != Some("object")
        {
            left.push(format!("{shown}: its input schema is not of type object"));
            continue;
        }
        if offered.len() == MAX_TOOLS {
            left.push(format!("{shown}: past a server's {MAX_TOOLS} tools"));
            continue;
        }
        let schema = Json::Obj(
            members
                .iter()
                .filter(|(key, _)| key != "$schema")
                .cloned()
                .collect(),
        );
        if schema.to_string().len() > MAX_SCHEMA {
            left.push(format!(
                "{shown}: its input schema is past {MAX_SCHEMA} bytes"
            ));
            continue;
        }
        if depth(&schema) > MAX_SCHEMA_DEPTH {
            left.push(format!(
                "{shown}: its input schema nests past {MAX_SCHEMA_DEPTH} levels"
            ));
            continue;
        }
        let description = quoted(
            tool.get("description")
                .and_then(Json::as_str)
                .unwrap_or_default(),
            MAX_DESCRIPTION,
        );
        let full = format!("mcp__{server}__{name}");
        offered.push(Offered {
            name: full.clone(),
            tool: name.to_string(),
            definition: Json::Obj(vec![
                ("type".into(), Json::Str("function".into())),
                (
                    "function".into(),
                    Json::Obj(vec![
                        ("name".into(), Json::Str(full)),
                        ("description".into(), Json::Str(description)),
                        ("parameters".into(), schema),
                    ]),
                ),
            ]),
        });
    }
    if let Some(keep) = keep {
        let mut named = BTreeSet::new();
        for wanted in keep {
            if !listed.contains(wanted.as_str()) && named.insert(wanted.as_str()) {
                left.push(format!(
                    "{}: the server lists no such tool",
                    one_line(wanted)
                ));
            }
        }
    }
    (offered, left.done())
}

/// How deep `value` nests: 1 for a scalar or an empty array or object.
/// What td-json parsed is at most its own depth, so this recursion is
/// bounded.
fn depth(value: &Json) -> usize {
    let below = match value {
        Json::Arr(items) => items.iter().map(depth).max().unwrap_or(0),
        Json::Obj(members) => members.iter().map(|(_, v)| depth(v)).max().unwrap_or(0),
        _ => 0,
    };
    below.saturating_add(1)
}

/// Every server's tools, in order, kept within `MAX_TOOLS` and
/// `MAX_TOOLS_BYTES` together: from the first that does not fit, every
/// later tool and server's is left out, and said for each server.
pub fn fit(servers: Vec<Vec<Offered>>) -> (Vec<Vec<Offered>>, Vec<Vec<String>>) {
    let (mut count, mut bytes, mut full) = (0usize, 0usize, false);
    let mut kept = Vec::with_capacity(servers.len());
    let mut said = Vec::with_capacity(servers.len());
    for tools in servers {
        let mut these = Vec::new();
        let mut left = Left::default();
        for tool in tools {
            let size = tool.size();
            full = full || count == MAX_TOOLS || bytes.saturating_add(size) > MAX_TOOLS_BYTES;
            if full {
                left.push(format!(
                    "{}: past every server's {MAX_TOOLS} tools or {MAX_TOOLS_BYTES} bytes",
                    tool.tool
                ));
            } else {
                count += 1;
                bytes += size;
                these.push(tool);
            }
        }
        kept.push(these);
        said.push(left.done());
    }
    (kept, said)
}

/// A `tools/call` result as the call's text, and whether it is an error:
/// its content in order, joined by blank lines, each image item the line
/// `picture` gives for it (the picture kept, or why not), what td-agent
/// does not show named; with no content, its structured content encoded.
pub fn result_text(result: &Json, picture: &mut dyn FnMut(&Json) -> String) -> (String, bool) {
    let error = result.get("isError").is_some_and(Json::is_true);
    let items = result
        .get("content")
        .and_then(Json::as_arr)
        .unwrap_or_default();
    if items.is_empty() {
        let text = result
            .get("structuredContent")
            .map(Json::to_string)
            .unwrap_or_default();
        return (text, error);
    }
    let parts: Vec<String> = items
        .iter()
        .map(|item| {
            let kind = item.get("type").and_then(Json::as_str).unwrap_or_default();
            match kind {
                "text" => item
                    .get("text")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_string(),
                "resource" => match item.get("resource").and_then(|r| r.get("text")) {
                    Some(Json::Str(text)) => text.clone(),
                    _ => "[a resource of bytes the tool returned, which td-agent does not show]"
                        .to_string(),
                },
                "resource_link" => {
                    let uri = item.get("uri").and_then(Json::as_str).unwrap_or_default();
                    format!("[a link the tool returned: {}]", one_line(uri))
                }
                "image" => picture(item),
                other => format!(
                    "[{} the tool returned, which td-agent does not show]",
                    match other {
                        "audio" => "audio".to_string(),
                        _ => format!("an item of kind {}", one_line(other)),
                    }
                ),
            }
        })
        .collect();
    (parts.join("\n\n"), error)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use std::collections::VecDeque;

    type Answer = Box<dyn FnMut(&Json) -> Vec<String>>;

    /// A server in the test: each request line td-agent sends is
    /// answered with what `answer` gives, which comes back as one piece.
    struct Fake {
        out: VecDeque<Vec<u8>>,
        sent: Vec<Json>,
        answer: Answer,
    }

    impl Fake {
        fn new(answer: impl FnMut(&Json) -> Vec<String> + 'static) -> Self {
            Self {
                out: VecDeque::new(),
                sent: Vec::new(),
                answer: Box::new(answer),
            }
        }
    }

    impl Transport for Fake {
        fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
            assert_eq!(bytes.last(), Some(&b'\n'));
            assert_eq!(bytes.iter().filter(|b| **b == b'\n').count(), 1);
            let message = td_json::parse_slice(&bytes[..bytes.len() - 1]).unwrap();
            let lines = (self.answer)(&message);
            self.sent.push(message);
            if !lines.is_empty() {
                self.out.push_back(lines.join("").into_bytes());
            }
            Ok(())
        }

        fn receive(&mut self, deadline: Instant) -> Result<Option<Vec<u8>>, String> {
            if let Some(piece) = self.out.pop_front() {
                return Ok(Some(piece));
            }
            std::thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(5)),
            );
            Ok(None)
        }
    }

    /// A deadline `ms` from now that does not move.
    fn in_ms(ms: u64) -> impl FnMut() -> Instant {
        let end = Instant::now() + Duration::from_millis(ms);
        move || end
    }

    fn soon() -> impl FnMut() -> Instant {
        in_ms(5000)
    }

    fn id(message: &Json) -> u64 {
        message.get("id").and_then(Json::as_u64).unwrap()
    }

    fn method(message: &Json) -> &str {
        message.get("method").and_then(Json::as_str).unwrap_or("")
    }

    fn reply(id: u64, result: &str) -> String {
        format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{result}}}\n")
    }

    fn tool(name: &str) -> String {
        format!(
            r#"{{"name":"{name}","description":"Does {name}.","inputSchema":{{"type":"object","properties":{{}}}}}}"#
        )
    }

    /// A server that starts, lists `pages` of tools, and answers calls
    /// with `called`.
    fn server(pages: Vec<Vec<&'static str>>) -> Fake {
        let mut page = 0;
        Fake::new(move |m| match method(m) {
            "initialize" => vec![reply(
                id(m),
                r#"{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"s","version":"1"}}"#,
            )],
            "tools/list" => {
                let tools: Vec<String> = pages[page].iter().map(|n| tool(n)).collect();
                page += 1;
                let next = if page < pages.len() {
                    format!(r#","nextCursor":"p{page}""#)
                } else {
                    String::new()
                };
                vec![reply(
                    id(m),
                    &format!(r#"{{"tools":[{}]{next}}}"#, tools.join(",")),
                )]
            }
            "tools/call" => vec![reply(
                id(m),
                r#"{"content":[{"type":"text","text":"done"}],"isError":false}"#,
            )],
            _ => Vec::new(),
        })
    }

    /// A server starts with `initialize`, `notifications/initialized` and
    /// the listing's pages, its cursor followed, then answers a call.
    #[test]
    fn a_server_starts_lists_its_pages_and_answers_a_call() {
        let mut s = Server::new(server(vec![vec!["a", "b"], vec!["c"]]));
        let started = s.start(&mut soon(), &mut || false).unwrap();
        assert_eq!(started.protocol, "2025-03-26");
        assert_eq!(started.tools.len(), 3);
        assert!(!started.more);
        let sent: Vec<&str> = s.transport.sent.iter().map(method).collect();
        assert_eq!(
            sent,
            [
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/list"
            ]
        );
        let init = &s.transport.sent[0];
        assert_eq!(
            init.get_path(&["params", "protocolVersion"])
                .and_then(Json::as_str),
            Some(PROTOCOL)
        );
        assert_eq!(
            init.get_path(&["params", "capabilities"])
                .unwrap()
                .to_string(),
            "{}"
        );
        assert_eq!(
            s.transport.sent[3]
                .get_path(&["params", "cursor"])
                .and_then(Json::as_str),
            Some("p1")
        );
        let result = s
            .call(
                "a",
                Json::Obj(Vec::new()),
                Duration::from_secs(5),
                &mut || false,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            result_text(&result, &mut |_| String::new()),
            ("done".to_string(), false)
        );
    }

    /// Past `MAX_PAGES` the listing stops and says more was left; a
    /// protocol td-agent does not speak, a reply that is no JSON-RPC, a
    /// batch, and a line past `MAX_LINE` end the server.
    #[test]
    fn a_server_that_breaks_the_protocol_is_ended() {
        let pages = vec![vec!["t"]; MAX_PAGES + 2];
        let mut s = Server::new(server(pages));
        let started = s.start(&mut soon(), &mut || false).unwrap();
        assert_eq!((started.tools.len(), started.more), (MAX_PAGES, true));
        let broke = |answer: &'static str| {
            let mut s = Server::new(Fake::new(move |m| match method(m) {
                "initialize" => vec![answer.replace("ID", &id(m).to_string())],
                _ => Vec::new(),
            }));
            match s.start(&mut soon(), &mut || false) {
                Err(Failure::Broke(why)) => why,
                other => panic!("{other:?}"),
            }
        };
        assert!(broke(
            "{\"jsonrpc\":\"2.0\",\"id\":ID,\"result\":{\"protocolVersion\":\"1999-01-01\"}}\n"
        )
        .contains("protocol 1999-01-01"));
        assert!(broke("{\"id\":ID,\"result\":{}}\n").contains("not JSON-RPC 2.0"));
        assert!(broke("[{\"jsonrpc\":\"2.0\",\"id\":ID,\"result\":{}}]\n").contains("batch"));
        assert!(broke("not json\n").contains("not JSON"));
        // The parser's words are made visible: a duplicate key with a
        // control in it is named, not written.
        let twice = broke("{\"jsonrpc\":\"2.0\",\"id\":ID,\"a\\u001b\":1,\"a\\u001b\":2}\n");
        assert!(!twice.contains('\u{1b}'), "{twice:?}");
        // An error for a request it could not read ends it at once.
        assert!(broke(
            "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32700,\"message\":\"Parse error\"}}\n"
        )
        .contains("could not read a request"));
        // A line of `MAX_LINE` is read, with a carriage return or without;
        // one byte more is not.
        let mut queued = VecDeque::new();
        for end in [&b"\n"[..], b"\r\n"] {
            let mut lines = Lines::default();
            lines.push(&vec![b'x'; MAX_LINE], &mut queued).unwrap();
            lines.push(end, &mut queued).unwrap();
        }
        assert_eq!(queued.len(), 2);
        let mut lines = Lines::default();
        lines.push(&vec![b'x'; MAX_LINE + 1], &mut queued).unwrap();
        assert!(lines.push(b"x", &mut queued).unwrap_err().contains("past"));
        let mut lines = Lines::default();
        lines.push(&vec![b'x'; MAX_LINE + 1], &mut queued).unwrap();
        assert!(lines.push(b"\n", &mut queued).unwrap_err().contains("past"));
        // A carriage return before the newline is dropped, an empty line
        // skipped, and a line may come in pieces.
        let (mut lines, mut queued) = (Lines::default(), VecDeque::new());
        lines.push(b"{\"a\"", &mut queued).unwrap();
        assert!(queued.is_empty());
        lines.push(b":1}\r\n\n{}\n", &mut queued).unwrap();
        assert_eq!(queued, [b"{\"a\":1}".to_vec(), b"{}".to_vec()]);
    }

    /// What follows an answer in the same piece is kept for the next
    /// read: a ping after it is answered, a tools change noted. An empty
    /// cursor ends a listing, and a call's result that is not an object
    /// ends the server.
    #[test]
    fn what_follows_an_answer_is_read_next() {
        let mut s = Server::new(Fake::new(|m| match method(m) {
            "tools/call" => vec![
                reply(id(m), r#"{"content":[]}"#),
                "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"ping\"}\n".into(),
                "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n".into(),
            ],
            "tools/list" => vec![reply(id(m), r#"{"tools":[],"nextCursor":""}"#)],
            _ => Vec::new(),
        }));
        s.call(
            "t",
            Json::Obj(Vec::new()),
            Duration::from_secs(5),
            &mut || false,
        )
        .unwrap()
        .unwrap();
        assert!(!s.list_changed);
        s.pump(Duration::from_millis(20)).unwrap();
        assert!(s.list_changed);
        assert!(s
            .transport
            .sent
            .iter()
            .any(|m| m.to_string() == r#"{"jsonrpc":"2.0","id":5,"result":{}}"#));
        let listed = s.list(&mut soon(), &mut || false);
        assert_eq!(listed, Ok((Vec::new(), false)));
        assert_eq!(
            s.transport
                .sent
                .iter()
                .filter(|m| method(m) == "tools/list")
                .count(),
            1
        );
        let mut s = Server::new(Fake::new(|m| vec![reply(id(m), "[1]")]));
        let answer = s.call(
            "t",
            Json::Obj(Vec::new()),
            Duration::from_secs(5),
            &mut || false,
        );
        assert!(matches!(answer, Err(Failure::Broke(why)) if why.contains("not an object")));
    }

    /// A server's requests are answered as it asks them: a ping with an
    /// empty result, anything else as a method not found; a tools change
    /// it says is noted; an answer to nothing asked is dropped.
    #[test]
    fn a_servers_requests_and_notifications_are_answered() {
        let mut asked = false;
        let mut s = Server::new(Fake::new(move |m| match method(m) {
            "tools/call" if !asked => {
                asked = true;
                vec![
                    "{\"jsonrpc\":\"2.0\",\"id\":\"p\",\"method\":\"ping\"}\n".into(),
                    "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"sampling/createMessage\",\"params\":{}}\n".into(),
                    "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n".into(),
                    "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{}}\n".into(),
                    reply(999, "{}"),
                    reply(id(m), r#"{"content":[]}"#),
                ]
            }
            _ => Vec::new(),
        }));
        let result = s
            .call(
                "t",
                Json::Obj(Vec::new()),
                Duration::from_secs(5),
                &mut || false,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            result_text(&result, &mut |_| String::new()),
            (String::new(), false)
        );
        assert!(s.list_changed);
        let answers: Vec<String> = s
            .transport
            .sent
            .iter()
            .filter(|m| m.get("method").is_none())
            .map(Json::to_string)
            .collect();
        assert_eq!(
            answers,
            [
                r#"{"jsonrpc":"2.0","id":"p","result":{}}"#,
                r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"Method not found"}}"#,
            ]
        );
    }

    /// A call stopped or out of time is cancelled, its late answer
    /// dropped; a server that leaves too many unanswered is ended; and
    /// `initialize` that never answers ends the server, uncancelled.
    #[test]
    fn a_call_stopped_or_late_is_cancelled_and_its_answer_dropped() {
        let mut late = Vec::new();
        let mut s = Server::new(Fake::new(move |m| match method(m) {
            "tools/call" => {
                late.push(id(m));
                Vec::new()
            }
            "notifications/cancelled" => {
                let id = m
                    .get_path(&["params", "requestId"])
                    .and_then(Json::as_u64)
                    .unwrap();
                vec![reply(
                    id,
                    r#"{"content":[{"type":"text","text":"too late"}]}"#,
                )]
            }
            _ => Vec::new(),
        }));
        let stopped = s.call(
            "t",
            Json::Obj(Vec::new()),
            Duration::from_secs(5),
            &mut || true,
        );
        assert_eq!(stopped, Err(Failure::Stopped));
        let timed = s.call(
            "t",
            Json::Obj(Vec::new()),
            Duration::from_millis(50),
            &mut || false,
        );
        assert_eq!(timed, Err(Failure::TimedOut));
        let cancels: Vec<&Json> = s
            .transport
            .sent
            .iter()
            .filter(|m| method(m) == "notifications/cancelled")
            .collect();
        assert_eq!(cancels.len(), 2);
        assert_eq!(
            cancels[0]
                .get_path(&["params", "reason"])
                .and_then(Json::as_str),
            Some("interrupted")
        );
        // The late answers are read, and dropped, while nothing waits.
        s.pump(Duration::from_millis(50)).unwrap();
        assert!(s.abandoned.is_empty());
        // One that never answers even its cancel is ended past the bound.
        let mut s = Server::new(Fake::new(|_| Vec::new()));
        for _ in 0..MAX_ABANDONED {
            assert_eq!(
                s.call(
                    "t",
                    Json::Obj(Vec::new()),
                    Duration::from_millis(1),
                    &mut || false
                ),
                Err(Failure::TimedOut)
            );
        }
        assert!(matches!(
            s.call("t", Json::Obj(Vec::new()), Duration::from_millis(1), &mut || false),
            Err(Failure::Broke(why)) if why.contains("cancelled requests")
        ));
        // A start the server's end stops ends it, and sends no cancel.
        let mut s = Server::new(Fake::new(|_| Vec::new()));
        assert!(matches!(
            s.start(&mut soon(), &mut || true),
            Err(Failure::Broke(why)) if why.contains("ended while td-agent waited for `initialize`")
        ));
        let mut s = Server::new(Fake::new(|_| Vec::new()));
        let late = s.list(&mut in_ms(50), &mut || false);
        assert!(matches!(late, Err(Failure::Broke(why)) if why.contains("`tools/list` in time")));
        assert!(s
            .transport
            .sent
            .iter()
            .all(|m| method(m) != "notifications/cancelled"));
    }

    /// Tools are offered as `mcp__SERVER__TOOL`, their descriptions
    /// visible and cut, their schemas without `$schema`; a tool whose
    /// name, schema or size does not fit, or that is listed twice, is
    /// left out and said; `keep` keeps only those named, and names one
    /// the server lacks.
    #[test]
    fn tools_are_checked_and_cut_as_offered() {
        let long = "d".repeat(MAX_DESCRIPTION + 10);
        let big = format!(
            r#"{{"type":"object","description":"{}"}}"#,
            "x".repeat(MAX_SCHEMA)
        );
        let tools: Vec<Json> = [
            r#"{"name":"read","description":"One\nTwo\u0007","inputSchema":{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"p":{"type":"string"}}}}"#.to_string(),
            format!(r#"{{"name":"long","description":"{long}","inputSchema":{{"type":"object"}}}}"#),
            tool("read"),
            tool("a.b"),
            tool(&"n".repeat(MAX_TOOL_NAME + 1)),
            r#"{"name":"arr","inputSchema":{"type":"array"}}"#.to_string(),
            r#"{"name":"none"}"#.to_string(),
            format!(r#"{{"name":"big","inputSchema":{big}}}"#),
        ]
        .iter()
        .map(|t| td_json::parse(t).unwrap())
        .collect();
        let (offered, left) = offer("gh", &tools, None);
        let names: Vec<&str> = offered.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["mcp__gh__read", "mcp__gh__long"]);
        let read = offered[0].definition.get("function").unwrap();
        assert_eq!(
            read.get("description").and_then(Json::as_str),
            Some("One\nTwo<U+0007>")
        );
        assert_eq!(
            read.get("parameters").unwrap().to_string(),
            r#"{"type":"object","properties":{"p":{"type":"string"}}}"#
        );
        assert_eq!(
            offered[1]
                .definition
                .get_path(&["function", "description"])
                .and_then(Json::as_str)
                .map(str::len),
            Some(MAX_DESCRIPTION)
        );
        assert_eq!(left.len(), 6, "{left:?}");
        for (said, why) in [
            (0, "listed twice"),
            (1, "ASCII"),
            (2, "ASCII"),
            (3, "not of type object"),
            (4, "not an object"),
            (5, "past"),
        ] {
            assert!(left[said].contains(why), "{}", left[said]);
        }
        let keep = vec!["long".to_string(), "absent".to_string()];
        let (offered, left) = offer("gh", &tools, Some(&keep));
        assert_eq!(offered.len(), 1);
        assert!(left
            .iter()
            .any(|l| l.contains("absent: the server lists no such tool")));
        // Together, the later servers' past the bound are left out.
        let many = |n: usize| -> Vec<Offered> {
            let tools: Vec<Json> = (0..n)
                .map(|i| td_json::parse(&tool(&format!("t{i}"))).unwrap())
                .collect();
            offer("s", &tools, None).0
        };
        let (kept, said) = fit(vec![many(100), many(40)]);
        assert_eq!((kept[0].len(), kept[1].len()), (100, MAX_TOOLS - 100));
        assert_eq!((said[0].len(), said[1].len()), (0, 140 - MAX_TOOLS));
    }

    /// A result's items become its text in order, what td-agent does not
    /// show named; `isError` makes it an error; with no content, its
    /// structured content; a JSON-RPC error names its code and message.
    #[test]
    fn a_result_becomes_the_calls_text() {
        let result = td_json::parse(
            r#"{"content":[{"type":"text","text":"one"},{"type":"image","data":"AAAA","mimeType":"image/png"},{"type":"resource","resource":{"uri":"file:///a","text":"two"}},{"type":"resource","resource":{"uri":"file:///b","blob":"AA"}},{"type":"resource_link","uri":"file:///c\u202e","name":"c"},{"type":"audio","data":""},{"type":"weird"}],"isError":true}"#,
        )
        .unwrap();
        let mut images = Vec::new();
        let (text, error) = result_text(&result, &mut |item| {
            images.push(item.get("data").and_then(Json::as_str).unwrap().to_string());
            "[tool image 1]".to_string()
        });
        assert_eq!(images, ["AAAA"]);
        assert!(error);
        assert_eq!(
            text,
            "one\n\n[tool image 1]\n\ntwo\n\n[a resource of bytes the tool returned, which td-agent does not show]\n\n[a link the tool returned: file:///c<U+202E>]\n\n[audio the tool returned, which td-agent does not show]\n\n[an item of kind weird the tool returned, which td-agent does not show]"
        );
        let structured = td_json::parse(r#"{"structuredContent":{"n":1}}"#).unwrap();
        assert_eq!(
            result_text(&structured, &mut |_| String::new()),
            (r#"{"n":1}"#.to_string(), false)
        );
        let error = RpcError {
            code: -32602,
            message: "bad\nargs".into(),
        };
        assert_eq!(
            error.to_string(),
            "the server answered error -32602: bad<U+000A>args"
        );
        assert!(matches!(
            parse(br#"{"jsonrpc":"2.0","id":3,"error":{"code":-32602,"message":"m"}}"#),
            Ok(Incoming::Response {
                id: Some(3),
                outcome: Err(RpcError { code: -32602, .. })
            })
        ));
        assert!(parse(br#"{"jsonrpc":"2.0","id":3}"#).is_err());
        assert!(parse(br#"{"jsonrpc":"2.0","id":3,"error":{"message":"m"}}"#).is_err());
    }

    /// What a hostile server lists is bounded: tools past `MAX_LISTED`
    /// are left, as pages past `MAX_PAGES` are; reasons past `MAX_LEFT`
    /// are counted, not said; one server offers at most `MAX_TOOLS`; a
    /// name listed again repeats the first listing even when that one was
    /// left out; a tool `keep` names twice and the server lacks is said
    /// once; together, everything from the first tool that does not fit
    /// is left; a line past `MAX_LINE` ends the server; and what is
    /// quoted is cut by characters, never within a name it made.
    #[test]
    fn what_a_hostile_server_lists_is_bounded() {
        let many: Vec<String> = (0..MAX_LISTED + 5)
            .map(|i| tool(&format!("t{i}")))
            .collect();
        let page = format!(r#"{{"tools":[{}]}}"#, many.join(","));
        let mut s = Server::new(Fake::new(move |m| vec![reply(id(m), &page)]));
        let (tools, more) = s.list(&mut soon(), &mut || false).unwrap();
        assert_eq!((tools.len(), more), (MAX_LISTED, true));
        let unnamed = vec![td_json::parse("{}").unwrap(); MAX_LEFT + 8];
        let (_, left) = offer("s", &unnamed, None);
        assert_eq!(left.len(), MAX_LEFT + 1);
        assert_eq!(left.last().map(String::as_str), Some("and 8 more"));
        let (offered, left) = offer("s", &tools, None);
        assert_eq!(offered.len(), MAX_TOOLS);
        assert_eq!(left.last().map(String::as_str), Some("and 864 more"));
        let twice: Vec<Json> = [
            r#"{"name":"x","inputSchema":{"type":"array"}}"#.to_string(),
            tool("x"),
        ]
        .iter()
        .map(|t| td_json::parse(t).unwrap())
        .collect();
        let keep = vec!["x".to_string(), "gone".to_string(), "gone".to_string()];
        let (offered, left) = offer("s", &twice, Some(&keep));
        assert!(offered.is_empty());
        assert_eq!(
            left,
            [
                "x: its input schema is not of type object",
                "x: listed twice",
                "gone: the server lists no such tool",
            ]
        );
        // A schema nests at most 64 levels: two objects and its arrays.
        let nested = |name: &str, arrays: usize| {
            let deep = format!("{}{}", "[".repeat(arrays), "]".repeat(arrays));
            td_json::parse(&format!(
                r#"{{"name":"{name}","inputSchema":{{"type":"object","properties":{deep}}}}}"#
            ))
            .unwrap()
        };
        let (offered, left) = offer(
            "s",
            &[
                nested("fits", MAX_SCHEMA_DEPTH - 1),
                nested("deep", MAX_SCHEMA_DEPTH),
            ],
            None,
        );
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].tool, "fits");
        assert_eq!(left, ["deep: its input schema nests past 64 levels"]);
        let sized = |name: &str, bytes: usize| Offered {
            name: format!("mcp__s__{name}"),
            tool: name.to_string(),
            definition: Json::Str("x".repeat(bytes)),
        };
        let (kept, said) = fit(vec![
            vec![sized("a", 100), sized("b", MAX_TOOLS_BYTES)],
            vec![sized("c", 100)],
        ]);
        assert_eq!((kept[0].len(), kept[1].len()), (1, 0));
        assert_eq!((said[0].len(), said[1].len()), (1, 1));
        let mut s = Server::new(Fake::new(|_| {
            vec![format!("{}\n", "x".repeat(MAX_LINE + 1))]
        }));
        assert!(matches!(
            s.start(&mut soon(), &mut || false),
            Err(Failure::Broke(why)) if why.contains("past")
        ));
        let long = one_line(&"\u{e9}".repeat(3 * MAX_QUOTED));
        assert_eq!(long.chars().count(), MAX_QUOTED);
        let names = one_line(&"\u{7}".repeat(MAX_QUOTED));
        assert!(names.ends_with('>') && names.len() <= MAX_QUOTED, "{names}");
        // The server's own `<U+` is named, so no text of its looks like a
        // name td-agent made, nor is cut as an unfinished one.
        assert_eq!(
            one_line("code <U+0041 and <U+202E> here"),
            "code <U+003C>U+0041 and <U+003C>U+202E> here"
        );
        assert!(quoted("points <U+0041 then more", 100).ends_with("then more"));
        let cut = quoted(&"a\u{7}".repeat(MAX_DESCRIPTION), MAX_DESCRIPTION);
        assert!(
            cut.ends_with('>') || cut.ends_with('a'),
            "{}",
            &cut[cut.len() - 10..]
        );
        assert!(cut.len() <= MAX_DESCRIPTION);
    }

    /// A transport over a socket, its reader on a thread of its own as a
    /// server's client reads its replies.
    struct Piped {
        stream: std::os::unix::net::UnixStream,
        pieces: std::sync::mpsc::Receiver<Vec<u8>>,
    }

    impl Transport for Piped {
        fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
            std::io::Write::write_all(&mut self.stream, bytes).map_err(|e| e.to_string())
        }

        fn receive(&mut self, deadline: Instant) -> Result<Option<Vec<u8>>, String> {
            let wait = deadline.saturating_duration_since(Instant::now());
            match self.pieces.recv_timeout(wait) {
                Ok(piece) => Ok(Some(piece)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(None),
                Err(_) => Err("the server is gone".into()),
            }
        }
    }

    /// Over a socket, a server that writes each answer in pieces, a ping
    /// of its own ahead of it, starts and answers a call; a start's clock
    /// stopped while it is slow lets it start past its budget; when it
    /// goes, the wait says so.
    #[test]
    fn a_server_over_a_socket_is_read_in_pieces() {
        use std::io::{BufRead, Read, Write};
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let (to_us, pieces) = std::sync::mpsc::channel();
        let mut reading = ours.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 7];
            while let Ok(n @ 1..) = reading.read(&mut buffer) {
                if to_us.send(buffer[..n].to_vec()).is_err() {
                    return;
                }
            }
        });
        // The server is slow while this is set, and its start's clock is
        // stopped while it is.
        let slow = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let slowing = slow.clone();
        let server = std::thread::spawn(move || {
            let mut writer = theirs.try_clone().unwrap();
            let mut answer = server(vec![vec!["a", "b"]]);
            let mut pinged = false;
            for line in std::io::BufReader::new(theirs).lines() {
                let message = td_json::parse(&line.unwrap()).unwrap();
                let mut out = String::new();
                if !pinged {
                    pinged = true;
                    // Slower than the start's budget, its clock stopped.
                    slowing.store(true, std::sync::atomic::Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(1000));
                    slowing.store(false, std::sync::atomic::Ordering::SeqCst);
                    out.push_str("{\"jsonrpc\":\"2.0\",\"id\":\"x\",\"method\":\"ping\"}\r\n");
                }
                out.push_str(&(answer.answer)(&message).concat());
                for piece in out.as_bytes().chunks(13) {
                    writer.write_all(piece).unwrap();
                    std::thread::sleep(Duration::from_millis(1));
                }
                if method(&message) == "tools/call" {
                    return;
                }
            }
        });
        let mut s = Server::new(Piped {
            stream: ours,
            pieces,
        });
        // Half a second, but for while the server is slow.
        let (begun, mut last, mut paused) = (Instant::now(), Instant::now(), Duration::ZERO);
        let mut deadline = move || {
            let now = Instant::now();
            if slow.load(std::sync::atomic::Ordering::SeqCst) {
                paused += now - last;
            }
            last = now;
            begun + Duration::from_millis(500) + paused
        };
        let started = s.start(&mut deadline, &mut || false).unwrap();
        assert_eq!(started.tools.len(), 2);
        let result = s
            .call(
                "b",
                Json::Obj(Vec::new()),
                Duration::from_secs(5),
                &mut || false,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            result_text(&result, &mut |_| String::new()),
            ("done".to_string(), false)
        );
        server.join().unwrap();
        // The server's socket closed with ours still open: its reader
        // sees the end, and the next wait says it is gone.
        let gone = s.pump(Duration::from_secs(5));
        assert!(matches!(gone, Err(Failure::Broke(_))), "{gone:?}");
    }
}
