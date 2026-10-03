//! A streamed reply put back together (DESIGN.md §5): each event of a
//! `stream: true` chat completion is one chunk, `{"choices":[{"delta":…,
//! "finish_reason":…}],"usage":…}`, and what its deltas carry is assembled
//! here into the completion a counted reply would have given.
//!
//! - **Text** (`delta.content`) and **reasoning** (`delta.reasoning`) are
//!   appended as they come; what came since the window was last told is
//!   handed over by `fresh`, so the window draws the reply as it arrives.
//! - **`reasoning_details`** fragments are joined as OpenRouter's own
//!   client (ai-sdk-provider) joins them: by the type's transitions, not
//!   by `index`, which providers reuse. A `reasoning.text` or
//!   `reasoning.summary` fragment joins the entry before it when that
//!   entry is of its type, appending its `text` or `summary`; where
//!   upstream fills only a missing `signature` and `format`, any member
//!   the entry lacks or holds as null or empty is filled here, the
//!   entry's first value of every other member standing. Any other
//!   fragment, an encrypted block always, is an entry of its own. The
//!   array is serialized once, when the reply completes, and stored as
//!   that text, which every later request sends back unchanged.
//! - **Tool calls** are assembled by `index`: `id`, `type` and the
//!   function's `name` from the first fragment that carries each, the
//!   function's `arguments` appended. A whole reply's every call must
//!   have come with an id and a name.
//! - **`finish_reason`** is the last one given and **`usage`** the last
//!   one given, which OpenRouter sends on the final chunk.
//!
//! Every chunk is checked for an `error` object, at the top or in its
//! choice, and for `finish_reason: "error"`: either ends the reply as an
//! error inside a 200 does (DESIGN.md §5), charged and offered again. What
//! is assembled is held to `MAX_REPLY` bytes as its log line will carry
//! it, escaped, so it fits one.

use crate::client::{self, Completion, Failure, Usage, MAX_REPLY};
use crate::json::Json;
use crate::store::Call;

/// The most reasoning-details entries, and tool calls, one reply holds.
pub const MAX_ENTRIES: usize = 256;

/// One tool call as its fragments assembled it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolCall {
    pub index: u64,
    pub id: Option<String>,
    pub kind: Option<String>,
    pub name: Option<String>,
    /// The function's arguments, a JSON text once whole.
    pub arguments: String,
}

/// A streamed reply so far.
#[derive(Clone, Debug, Default)]
pub struct Assembly {
    content: String,
    reasoning: String,
    details: Vec<Json>,
    calls: Vec<ToolCall>,
    finish: Option<String>,
    usage: Option<Usage>,
    /// What content and reasoning the window has been handed.
    shown: (usize, usize),
    /// The bytes assembled, as logged, against `MAX_REPLY`.
    bytes: usize,
}

/// What `text` takes in a log line: JSON-escaped once, or twice for the
/// text inside reasoning details, which are stored as a JSON string.
fn logged(text: &str, twice: bool) -> usize {
    text.chars()
        .map(|c| match c {
            '"' | '\\' if twice => 4,
            '"' | '\\' => 2,
            '\n' | '\r' | '\t' | '\u{08}' | '\u{0c}' if twice => 3,
            '\n' | '\r' | '\t' | '\u{08}' | '\u{0c}' => 2,
            c if (c as u32) < 0x20 && twice => 7,
            c if (c as u32) < 0x20 => 6,
            c => c.len_utf8(),
        })
        .fold(0, usize::saturating_add)
}

/// A failure inside the stream: an error the provider sent, or a chunk
/// that cannot be read as one.
fn failed(message: impl Into<String>, usage: Option<Usage>) -> Failure {
    Failure::Retryable {
        status: Some(200),
        message: message.into(),
        usage,
    }
}

impl Assembly {
    /// One event's text, a chunk of the completion.
    pub fn event(&mut self, data: &str) -> Result<(), Failure> {
        let chunk = crate::json::parse(data)
            .map_err(|e| failed(format!("a stream event is not JSON: {e}"), self.usage))?;
        if let Some(usage) = client::usage(&chunk) {
            self.usage = Some(usage);
        }
        if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
            return Err(self.error(error));
        }
        // The final chunk may carry usage and no choice at all.
        let Some(choice) = chunk.get("choices").and_then(|c| c.index(0)) else {
            return Ok(());
        };
        if let Some(error) = choice.get("error").filter(|e| !e.is_null()) {
            return Err(self.error(error));
        }
        if let Some(delta) = choice.get("delta").filter(|d| !d.is_null()) {
            self.delta(delta)?;
        }
        match choice.get("finish_reason") {
            None | Some(Json::Null) => {}
            Some(Json::Str(reason)) if reason == "error" => {
                return Err(failed(
                    "the provider ended the completion with an error",
                    self.usage,
                ))
            }
            Some(Json::Str(reason)) => self.finish = Some(reason.clone()),
            Some(_) => return Err(failed("a finish_reason that is not text", self.usage)),
        }
        Ok(())
    }

    fn error(&self, error: &Json) -> Failure {
        let (code, message) = client::error_object(error);
        Failure::Retryable {
            status: code.or(Some(200)),
            message,
            usage: self.usage,
        }
    }

    /// Counts `more` bytes against the reply's bound.
    fn grow(&mut self, more: usize) -> Result<(), Failure> {
        self.bytes = self.bytes.saturating_add(more);
        if self.bytes > MAX_REPLY as usize {
            return Err(failed(
                format!("the reply passed {MAX_REPLY} bytes"),
                self.usage,
            ));
        }
        Ok(())
    }

    fn delta(&mut self, delta: &Json) -> Result<(), Failure> {
        for (name, text) in [("content", true), ("reasoning", false)] {
            match delta.get(name) {
                None | Some(Json::Null) => {}
                Some(Json::Str(more)) => {
                    self.grow(logged(more, false))?;
                    if text {
                        self.content.push_str(more);
                    } else {
                        self.reasoning.push_str(more);
                    }
                }
                Some(_) => return Err(failed(format!("a delta's {name} is not text"), self.usage)),
            }
        }
        match delta.get("reasoning_details") {
            None | Some(Json::Null) => {}
            Some(Json::Arr(fragments)) => {
                for fragment in fragments {
                    self.detail(fragment)?;
                }
            }
            Some(_) => return Err(failed("reasoning_details is not a list", self.usage)),
        }
        match delta.get("tool_calls") {
            None | Some(Json::Null) => {}
            Some(Json::Arr(fragments)) => {
                for fragment in fragments {
                    self.call(fragment)?;
                }
            }
            Some(_) => return Err(failed("tool_calls is not a list", self.usage)),
        }
        Ok(())
    }

    /// One `reasoning_details` fragment: joined to the entry before it,
    /// or an entry of its own.
    fn detail(&mut self, fragment: &Json) -> Result<(), Failure> {
        let Json::Obj(members) = fragment else {
            return Err(failed(
                "a reasoning detail that is not an object",
                self.usage,
            ));
        };
        let kind = fragment.get("type").and_then(Json::as_str);
        let joins = matches!(kind, Some("reasoning.text" | "reasoning.summary"))
            && self
                .details
                .last()
                .is_some_and(|entry| entry.get("type").and_then(Json::as_str) == kind);
        let entry = match self.details.last_mut() {
            Some(entry) if joins => entry,
            _ => {
                if self.details.len() >= MAX_ENTRIES {
                    return Err(failed(
                        format!("more than {MAX_ENTRIES} reasoning details"),
                        self.usage,
                    ));
                }
                // The entry, and the comma before it, in the stored text.
                let more = logged(&fragment.to_string(), false).saturating_add(1);
                self.grow(more)?;
                self.details.push(fragment.clone());
                return Ok(());
            }
        };
        let mut more = 0usize;
        for (key, value) in members {
            match (key.as_str(), entry.get_mut(key)) {
                ("text" | "summary", Some(Json::Str(have))) => {
                    if let Json::Str(text) = value {
                        have.push_str(text);
                        more = more.saturating_add(logged(text, true));
                    }
                }
                (_, Some(slot)) => {
                    // Unset as upstream's `||` reads it: an empty signature
                    // gives way to the real one that follows.
                    let unset = |v: &Json| v.is_null() || v.as_str() == Some("");
                    if unset(slot) && !unset(value) {
                        *slot = value.clone();
                        more = more.saturating_add(logged(&value.to_string(), false));
                    }
                }
                (_, None) => {
                    entry.insert(key.clone(), value.clone());
                    let member = format!(",{}:{value}", Json::Str(key.clone()));
                    more = more.saturating_add(logged(&member, false));
                }
            }
        }
        self.grow(more)
    }

    /// One tool-call fragment, assembled by its index.
    fn call(&mut self, fragment: &Json) -> Result<(), Failure> {
        let index = fragment
            .get("index")
            .and_then(Json::as_u64)
            .ok_or_else(|| failed("a tool call with no index", self.usage))?;
        let text = |value: Option<&Json>| value.and_then(Json::as_str).map(str::to_string);
        let function = fragment.get("function");
        let arguments = function
            .and_then(|f| f.get("arguments"))
            .and_then(Json::as_str)
            .unwrap_or_default();
        let at = match self.calls.iter().position(|call| call.index == index) {
            Some(at) => at,
            None => {
                if self.calls.len() >= MAX_ENTRIES {
                    return Err(failed(
                        format!("more than {MAX_ENTRIES} tool calls"),
                        self.usage,
                    ));
                }
                self.calls.push(ToolCall {
                    index,
                    ..ToolCall::default()
                });
                self.calls.len() - 1
            }
        };
        let Some(call) = self.calls.get(at) else {
            return Ok(());
        };
        // Counted as kept and logged, escaped: the first id, type and
        // name stand.
        let keep = |have: &Option<String>, given: Option<String>| given.filter(|_| have.is_none());
        let [id, kind, name] = [
            keep(&call.id, text(fragment.get("id"))),
            keep(&call.kind, text(fragment.get("type"))),
            keep(&call.name, text(function.and_then(|f| f.get("name")))),
        ];
        let kept = [&id, &kind, &name]
            .into_iter()
            .flatten()
            .map(|t| logged(t, false));
        self.grow(kept.fold(logged(arguments, false), usize::saturating_add))?;
        let Some(call) = self.calls.get_mut(at) else {
            return Ok(());
        };
        call.id = call.id.take().or(id);
        call.kind = call.kind.take().or(kind);
        call.name = call.name.take().or(name);
        call.arguments.push_str(arguments);
        Ok(())
    }

    /// The reasoning and the text that came since the last call, in that
    /// order.
    pub fn fresh(&mut self) -> (&str, &str) {
        let (content, reasoning) = self.shown;
        self.shown = (self.content.len(), self.reasoning.len());
        (
            self.reasoning.get(reasoning..).unwrap_or_default(),
            self.content.get(content..).unwrap_or_default(),
        )
    }

    /// Whether a `finish_reason` has come.
    pub fn finished(&self) -> bool {
        self.finish.is_some()
    }

    pub fn usage(&self) -> Option<Usage> {
        self.usage
    }

    /// The tool calls asked for, in the order their indices first came.
    pub fn calls(&self) -> &[ToolCall] {
        &self.calls
    }

    /// Whether nothing worth keeping has come: no text, reasoning,
    /// reasoning details or tool calls.
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
            && self.reasoning.is_empty()
            && self.details.is_empty()
            && self.calls.is_empty()
    }

    /// The reply as a completion; its `reasoning_details` serialized here,
    /// once. A call that came without an id or name has it empty.
    pub fn completion(&self) -> Completion {
        let some = |text: &str| (!text.is_empty()).then(|| text.to_string());
        Completion {
            content: some(&self.content),
            reasoning: some(&self.reasoning),
            details: (!self.details.is_empty())
                .then(|| Json::Arr(self.details.clone()).to_string()),
            finish: self.finish.clone().unwrap_or_else(|| "unknown".into()),
            usage: self.usage,
            calls: self
                .calls
                .iter()
                .map(|call| Call {
                    id: call.id.clone().unwrap_or_default(),
                    name: call.name.clone().unwrap_or_default(),
                    arguments: call.arguments.clone(),
                })
                .collect(),
        }
    }

    /// The whole reply as a completion: every tool call must pass
    /// `client::check_calls`, or the reply is a failure inside a 200,
    /// charged and offered again.
    pub fn whole(&self) -> Result<Completion, Failure> {
        let completion = self.completion();
        client::check_calls(&completion.calls).map_err(|e| failed(e, self.usage))?;
        Ok(completion)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use crate::cost::Tokens;
    use crate::sse::{Event, Reader, MAX_EVENT};

    /// A stream's events through the reader into an assembly.
    fn assemble(stream: &str) -> Result<Assembly, Failure> {
        let mut reader = Reader::new(MAX_EVENT, 1 << 20);
        let mut reply = Assembly::default();
        let result = reader.feed(stream.as_bytes(), &mut |event| match event {
            Event::Data(text) => reply.event(text),
            Event::Done => Ok(()),
        });
        match result {
            Ok(()) => Ok(reply),
            Err(crate::sse::Fault::Sink(failure)) => Err(failure),
            Err(crate::sse::Fault::Reader(e)) => panic!("{e}"),
        }
    }

    fn chunk(delta: &str) -> String {
        format!("data: {{\"id\":\"gen-1\",\"choices\":[{{\"index\":0,\"delta\":{delta},\"finish_reason\":null}}]}}\n\n")
    }

    #[test]
    fn text_and_reasoning_deltas_are_assembled_and_handed_over_once() {
        let mut stream = String::from(": OPENROUTER PROCESSING\n\n");
        stream += &chunk(r#"{"role":"assistant","content":"","reasoning":"Think"}"#);
        stream += &chunk(r#"{"content":null,"reasoning":"ing."}"#);
        let mut reply = assemble(&stream).unwrap();
        assert_eq!(reply.fresh(), ("Thinking.", ""));
        assert_eq!(reply.fresh(), ("", ""));
        reply
            .event(r#"{"choices":[{"delta":{"content":"Hello, é"}}]}"#)
            .unwrap();
        reply
            .event(r#"{"choices":[{"delta":{"content":"!"},"finish_reason":"stop"}]}"#)
            .unwrap();
        assert_eq!(reply.fresh(), ("", "Hello, \u{e9}!"));
        assert!(reply.finished());
        // The final chunk: usage and no choice.
        reply
            .event(r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4,"cost":0.0002}}"#)
            .unwrap();
        let completion = reply.completion();
        assert_eq!(completion.content.as_deref(), Some("Hello, \u{e9}!"));
        assert_eq!(completion.reasoning.as_deref(), Some("Thinking."));
        assert_eq!(completion.details, None);
        assert_eq!(completion.finish, "stop");
        assert_eq!(
            completion.usage,
            Some(Usage {
                tokens: Tokens {
                    prompt: 10,
                    completion: 4,
                    ..Tokens::default()
                },
                cost: Some(200_000_000),
            })
        );
    }

    /// Consecutive text fragments join into one entry, whose text grows
    /// and whose signature fills in late; an encrypted block, and a change
    /// of type, start another; the array serializes once, in the order its
    /// entries came.
    #[test]
    fn reasoning_details_join_and_serialize_once() {
        let mut stream = String::new();
        for delta in [
            r#"{"reasoning":"Plan","reasoning_details":[{"type":"reasoning.text","text":"Plan","format":"anthropic-claude-v1","index":0,"signature":null}]}"#,
            r#"{"reasoning":" it.","reasoning_details":[{"type":"reasoning.text","text":" it.","format":"anthropic-claude-v1","index":0}]}"#,
            r#"{"reasoning_details":[{"type":"reasoning.text","signature":"c2ln\/bmF0dXJl","index":0}]}"#,
            r#"{"reasoning_details":[{"type":"reasoning.encrypted","data":"QUJD","id":"r1","format":"google-gemini-v1","index":1}]}"#,
            r#"{"reasoning_details":[{"type":"reasoning.summary","summary":"Short","index":2},{"type":"reasoning.summary","summary":"er.","index":2}]}"#,
            r#"{"reasoning_details":[{"type":"reasoning.text","text":"no index"}]}"#,
            r#"{"content":"Done."}"#,
        ] {
            stream += &chunk(delta);
        }
        stream += "data: [DONE]\n\n";
        let reply = assemble(&stream).unwrap();
        let completion = reply.completion();
        assert_eq!(
            completion.details.as_deref(),
            Some(
                "[{\"type\":\"reasoning.text\",\"text\":\"Plan it.\",\"format\":\"anthropic-claude-v1\",\"index\":0,\"signature\":\"c2ln/bmF0dXJl\"},\
                 {\"type\":\"reasoning.encrypted\",\"data\":\"QUJD\",\"id\":\"r1\",\"format\":\"google-gemini-v1\",\"index\":1},\
                 {\"type\":\"reasoning.summary\",\"summary\":\"Shorter.\",\"index\":2},\
                 {\"type\":\"reasoning.text\",\"text\":\"no index\"}]"
            )
        );
        assert_eq!(completion.reasoning.as_deref(), Some("Plan it."));
        // A finish never given reads as unknown.
        assert_eq!(completion.finish, "unknown");
        // The serialized array is what the log stores and sends back.
        crate::json::parse(completion.details.as_deref().unwrap()).unwrap();
    }

    /// Providers reuse an index for distinct blocks: consecutive text or
    /// summary of one type join whatever their index, an encrypted block
    /// never does, and a value that differs from delta to delta does not
    /// split a block, the first one set standing (an empty one is unset).
    #[test]
    fn distinct_blocks_that_share_an_index_stay_distinct() {
        let mut stream = String::new();
        for delta in [
            r#"{"reasoning_details":[{"type":"reasoning.summary","summary":"A","index":0}]}"#,
            r#"{"reasoning_details":[{"type":"reasoning.encrypted","data":"aaa","id":"rs_1","index":0}]}"#,
            r#"{"reasoning_details":[{"type":"reasoning.summary","summary":"B","index":0}]}"#,
            r#"{"reasoning_details":[{"type":"reasoning.summary","summary":"C","index":0}]}"#,
            r#"{"reasoning_details":[{"type":"reasoning.encrypted","data":"bbb","id":"rs_2","index":0}]}"#,
            r#"{"reasoning_details":[{"type":"reasoning.encrypted","data":"ccc","id":"rs_3","index":0}]}"#,
            r#"{"reasoning_details":[{"type":"reasoning.text","text":"w","signature":"","index":1}]}"#,
            r#"{"reasoning_details":[{"type":"reasoning.text","text":"x","signature":"s1","index":1}]}"#,
            r#"{"reasoning_details":[{"type":"reasoning.text","text":"y","signature":"s2","index":1}]}"#,
            r#"{"reasoning_details":[{"type":"reasoning.text","text":"z","signature":null,"index":2}]}"#,
        ] {
            stream += &chunk(delta);
        }
        let completion = assemble(&stream).unwrap().completion();
        assert_eq!(
            completion.details.as_deref(),
            Some(
                "[{\"type\":\"reasoning.summary\",\"summary\":\"A\",\"index\":0},\
                 {\"type\":\"reasoning.encrypted\",\"data\":\"aaa\",\"id\":\"rs_1\",\"index\":0},\
                 {\"type\":\"reasoning.summary\",\"summary\":\"BC\",\"index\":0},\
                 {\"type\":\"reasoning.encrypted\",\"data\":\"bbb\",\"id\":\"rs_2\",\"index\":0},\
                 {\"type\":\"reasoning.encrypted\",\"data\":\"ccc\",\"id\":\"rs_3\",\"index\":0},\
                 {\"type\":\"reasoning.text\",\"text\":\"wxyz\",\"signature\":\"s1\",\"index\":1}]"
            )
        );
    }

    /// The bound is what the log line will carry: escaping counted, and a
    /// fragment's repeated members not.
    #[test]
    fn the_bound_counts_what_is_logged() {
        assert_eq!(logged("a\u{e9}\"\n\u{1}", false), 1 + 2 + 2 + 2 + 6);
        assert_eq!(logged("a\u{e9}\"\n\u{1}", true), 1 + 2 + 4 + 3 + 7);
        // A twice-escaped text is the escaping of its escaping.
        let text = "q\"b\\n\n\t\u{1f}\u{e9}";
        let once = Json::Str(text.into()).to_string();
        let twice = Json::Str(once.clone()).to_string();
        assert_eq!(logged(text, false), once.len() - 2);
        assert_eq!(logged(text, true), twice.len() - 2 - 4);
        // 16 Ki one-byte reasoning deltas, each carrying its type, format
        // and index, keep 16 KiB and are well within the bound.
        let mut reply = Assembly::default();
        let delta = chunk(
            r#"{"reasoning_details":[{"type":"reasoning.text","text":"x","format":"anthropic-claude-v1","index":0}]}"#,
        );
        let data = delta.trim_start_matches("data: ").trim_end();
        for _ in 0..16 * 1024 {
            reply.event(data).unwrap();
        }
        assert!(reply.bytes < 17 * 1024, "{}", reply.bytes);
        // Control characters pass it at a sixth of the raw bytes.
        let mut reply = Assembly::default();
        let event = format!(
            "{{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}}}}]}}",
            "\\u0001".repeat(16 * 1024)
        );
        for _ in 0..5 {
            reply.event(&event).unwrap();
        }
        assert!(reply.event(&event).is_err());
    }

    #[test]
    fn fragmented_tool_calls_are_assembled_by_index() {
        let mut stream = String::new();
        for delta in [
            r#"{"content":null,"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read_file","arguments":""}}]}"#,
            r#"{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}"#,
            r#"{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"grep","arguments":"{\"pattern\":"}}]}"#,
            r#"{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"/a\"}"}},{"index":1,"function":{"arguments":"\"x\"}"}}]}"#,
        ] {
            stream += &chunk(delta);
        }
        stream += "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n";
        let reply = assemble(&stream).unwrap();
        assert_eq!(
            reply.calls(),
            [
                ToolCall {
                    index: 0,
                    id: Some("call_a".into()),
                    kind: Some("function".into()),
                    name: Some("read_file".into()),
                    arguments: "{\"path\":\"/a\"}".into(),
                },
                ToolCall {
                    index: 1,
                    id: Some("call_b".into()),
                    kind: Some("function".into()),
                    name: Some("grep".into()),
                    arguments: "{\"pattern\":\"x\"}".into(),
                },
            ]
        );
        for call in reply.calls() {
            crate::json::parse(&call.arguments).unwrap();
        }
        assert_eq!(reply.completion().finish, "tool_calls");
        assert_eq!(reply.completion().content, None);
        let whole = reply.whole().unwrap();
        let names: Vec<(&str, &str)> = whole
            .calls
            .iter()
            .map(|c| (c.id.as_str(), c.name.as_str()))
            .collect();
        assert_eq!(names, [("call_a", "read_file"), ("call_b", "grep")]);
        // A call with no id cannot be answered, nor one with no name run.
        for delta in [
            r#"{"tool_calls":[{"index":0,"function":{"name":"grep","arguments":"{}"}}]}"#,
            r#"{"tool_calls":[{"index":0,"id":"call_c","function":{"arguments":"{}"}}]}"#,
        ] {
            let stream = chunk(delta)
                + "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n";
            let reply = assemble(&stream).unwrap();
            let Err(Failure::Retryable { message, .. }) = reply.whole() else {
                panic!("whole: {delta}")
            };
            assert!(message.starts_with("tool call 0 came without"), "{message}");
        }
        // A fragment with no index cannot be placed.
        let mut reply = Assembly::default();
        assert!(reply
            .event(r#"{"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"{}"}}]}}]}"#)
            .is_err());
    }

    /// An error object in a chunk ends the reply as one inside a 200,
    /// keeping what came before it and the usage it carried.
    #[test]
    fn an_error_mid_stream_ends_the_reply() {
        let mut stream = chunk(r#"{"content":"Half an ans"}"#);
        stream += "data: {\"id\":\"gen-1\",\"error\":{\"code\":502,\"message\":\"Provider disconnected unexpectedly\"},\
                   \"choices\":[{\"index\":0,\"delta\":{\"content\":\"\"},\"finish_reason\":\"error\"}],\
                   \"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"cost\":0.001}}\n\n";
        let mut reply = Assembly::default();
        let mut reader = Reader::new(MAX_EVENT, 1 << 20);
        let result = reader.feed(stream.as_bytes(), &mut |event| match event {
            Event::Data(text) => reply.event(text),
            Event::Done => Ok(()),
        });
        let Err(crate::sse::Fault::Sink(failure)) = result else {
            panic!("no failure: {result:?}")
        };
        assert!(matches!(
            failure,
            Failure::Retryable { status: Some(502), ref message, usage: Some(Usage { cost: Some(1_000_000_000), .. }) }
                if message == "Provider disconnected unexpectedly"
        ));
        assert_eq!(reply.completion().content.as_deref(), Some("Half an ans"));
        // A string code, a choice's error, and a bare error finish.
        for (event, status, said) in [
            (
                r#"{"error":{"code":"server_error","message":"gone"}}"#,
                200,
                "gone",
            ),
            (
                r#"{"choices":[{"error":{"code":503,"message":"busy"},"delta":{}}]}"#,
                503,
                "busy",
            ),
            (
                r#"{"choices":[{"delta":{},"finish_reason":"error"}]}"#,
                200,
                "the provider ended the completion with an error",
            ),
            ("not json", 200, "a stream event is not JSON"),
        ] {
            let failure = Assembly::default().event(event).unwrap_err();
            assert!(
                matches!(failure, Failure::Retryable { status: Some(s), ref message, .. }
                    if s == status && message.starts_with(said)),
                "{event}: {failure:?}"
            );
        }
    }

    #[test]
    fn the_assembled_reply_is_bounded() {
        let mut reply = Assembly::default();
        let piece = "x".repeat(64 * 1024);
        let event = format!("{{\"choices\":[{{\"delta\":{{\"content\":\"{piece}\"}}}}]}}");
        for _ in 0..8 {
            reply.event(&event).unwrap();
        }
        let failure = reply.event(&event).unwrap_err();
        assert!(
            matches!(failure, Failure::Retryable { ref message, .. } if message.contains("passed")),
            "{failure:?}"
        );
        let mut reply = Assembly::default();
        let many: Vec<String> = (0..=MAX_ENTRIES)
            .map(|i| format!("{{\"type\":\"reasoning.encrypted\",\"data\":\"\",\"index\":{i}}}"))
            .collect();
        let event = format!(
            "{{\"choices\":[{{\"delta\":{{\"reasoning_details\":[{}]}}}}]}}",
            many.join(",")
        );
        let failure = reply.event(&event).unwrap_err();
        assert!(
            matches!(failure, Failure::Retryable { ref message, .. }
                if message == "more than 256 reasoning details"),
            "{failure:?}"
        );
    }
}
