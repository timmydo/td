//! The model client's wire format (DESIGN.md §5): OpenAI Chat Completions
//! as OpenRouter serves it, non-streaming and with no tools in this
//! increment. What a request carries and how a reply reads are pure
//! functions here; `conversation` sends them through the fetch service.
//!
//! A request's body is `{HEAD,"messages":[PREFIX...,MESSAGES...]}`: `HEAD`
//! the exact text of its other members, logged with the request; the
//! prefix's messages as the conversation's prefix holds them; and one
//! message per user message and assistant reply logged before it. So
//! every request is a pure function of the prefix and the log, and a
//! restart sends the same bytes as before, which keeps a provider's
//! prompt cache warm. An assistant reply's `reasoning_details` go back as
//! the exact bytes the response carried them in (`span`), never
//! re-encoded.

use std::time::Duration;

use crate::config::Client;
use crate::cost::{self, Tokens};
use crate::json::Json;
use crate::span::{self, Step};
use crate::store::{Event, Kind, Purpose};
use crate::td_fetch;

/// The attribution pair's values (DESIGN.md §5).
pub const REFERER: &str = "https://github.com/timmydo/td";
pub const TITLE: &str = "td-agent";
/// The most a model reply may run to, which a log line holds.
pub const MAX_REPLY: u64 = 512 * 1024;
/// The completion bound a turn asks for, under the model's own.
pub const MAX_TOKENS: u64 = 16_384;
/// A title request's completion bound, and the most of the first
/// exchange it quotes.
pub const TITLE_TOKENS: u64 = 256;
const TITLE_QUOTE: usize = 4096;
/// Retries of a rate-limited request (DESIGN.md §5), and the longest wait
/// before one: a `Retry-After` past it stops the turn instead.
pub const RETRIES: u32 = 3;
pub const MAX_WAIT: Duration = Duration::from_secs(60);
/// The longest a provider's error message is shown.
const MAX_MESSAGE: usize = 500;

/// The headers of a model request, the key among them. Nothing else ever
/// holds the key outside the socketpair's frame and this list.
pub fn headers(key: &str) -> Vec<(&'static str, String)> {
    vec![
        ("authorization", format!("Bearer {key}")),
        ("content-type", "application/json".into()),
        ("http-referer", REFERER.into()),
        ("x-openrouter-title", TITLE.into()),
    ]
}

/// What a turn request asks for besides its messages.
pub struct Params<'a> {
    pub model: &'a str,
    pub max_tokens: u64,
    /// `reasoning.effort`, when the model takes the parameter.
    pub effort: Option<&'a str>,
    pub client: &'a Client,
}

/// `provider`: never routed to a provider that would drop a parameter,
/// and data collection as configured.
fn provider(client: &Client) -> Json {
    Json::Obj(vec![
        ("require_parameters".into(), Json::Bool(true)),
        (
            "data_collection".into(),
            Json::Str(
                if client.allow_data_collection {
                    "allow"
                } else {
                    "deny"
                }
                .into(),
            ),
        ),
    ])
}

/// An object's members without its braces, to be spliced.
fn members(object: Json) -> String {
    let text = object.to_string();
    text.strip_prefix('{')
        .and_then(|t| t.strip_suffix('}'))
        .unwrap_or_default()
        .to_string()
}

/// A turn request's head.
pub fn head(params: &Params<'_>) -> String {
    let mut pairs = vec![
        ("model".into(), Json::Str(params.model.into())),
        ("max_tokens".into(), Json::from(params.max_tokens)),
    ];
    if let Some(effort) = params.effort {
        pairs.push((
            "reasoning".into(),
            Json::Obj(vec![("effort".into(), Json::Str(effort.into()))]),
        ));
    }
    pairs.push(("provider".into(), provider(params.client)));
    // Anthropic's models cache only where asked (DESIGN.md §5).
    if params.model.starts_with("anthropic/") {
        pairs.push((
            "cache_control".into(),
            Json::Obj(vec![("type".into(), Json::Str("ephemeral".into()))]),
        ));
    }
    members(Json::Obj(pairs))
}

/// A title request's head, which holds its messages too: the title
/// prompt and a quotation of the first exchange (DESIGN.md §13).
pub fn title_head(client: &Client, user: &str, reply: &str) -> String {
    let quote = |text: &str| -> String {
        let mut cut = text.len().min(TITLE_QUOTE);
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.get(..cut).unwrap_or_default().to_string()
    };
    let exchange = format!(
        "The person wrote:\n{}\n\nThe reply began:\n{}",
        quote(user),
        quote(reply)
    );
    let message = |role: &str, content: &str| {
        Json::Obj(vec![
            ("role".into(), Json::Str(role.into())),
            ("content".into(), Json::Str(content.into())),
        ])
    };
    members(Json::Obj(vec![
        ("model".into(), Json::Str(client.title_model.clone())),
        ("max_tokens".into(), Json::from(TITLE_TOKENS)),
        ("provider".into(), provider(client)),
        (
            "messages".into(),
            Json::Arr(vec![
                message("system", crate::prompt::TITLE.trim_end()),
                message("user", &exchange),
            ]),
        ),
    ]))
}

/// The messages of the log before a request, as the request carries
/// them: each user message, and each turn reply that has text or
/// reasoning to give back.
pub fn messages(events: &[Event]) -> Vec<String> {
    let mut purposes: Vec<(u64, Purpose)> = Vec::new();
    let mut out = Vec::new();
    for event in events {
        match &event.kind {
            Kind::Request { purpose, .. } => purposes.push((event.seq, *purpose)),
            Kind::User { text, .. } => out.push(crate::prompt::message("user", text)),
            Kind::Assistant {
                request,
                content,
                details,
                ..
            } => {
                let turn = purposes
                    .iter()
                    .rev()
                    .find(|(seq, _)| seq == request)
                    .is_some_and(|(_, p)| *p == Purpose::Turn);
                let content = content.as_deref().unwrap_or_default();
                if !turn || (content.is_empty() && details.is_none()) {
                    continue;
                }
                let mut message = format!(
                    "{{\"role\":\"assistant\",\"content\":{}",
                    Json::Str(content.into())
                );
                if let Some(details) = details {
                    message.push_str(",\"reasoning_details\":");
                    message.push_str(details);
                }
                message.push('}');
                out.push(message);
            }
            _ => {}
        }
    }
    out
}

/// The prefix a turn request at `prefix` begins with: the file's text, or
/// the `Prefix` event that sequence number names.
pub fn prefix_text<'a>(events: &'a [Event], file: &'a str, prefix: u64) -> Option<&'a str> {
    if prefix == 0 {
        return Some(file);
    }
    events.iter().find_map(|e| match &e.kind {
        Kind::Prefix { text } if e.seq == prefix => Some(text.as_str()),
        _ => None,
    })
}

/// The prefix in force at the end of `events`: its sequence number (0
/// for the file) and text.
pub fn current_prefix<'a>(events: &'a [Event], file: &'a str) -> (u64, &'a str) {
    events
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            Kind::Prefix { text } => Some((e.seq, text.as_str())),
            _ => None,
        })
        .unwrap_or((0, file))
}

/// A turn body from its head, its prefix and the messages before it.
pub fn turn_body(head: &str, prefix: &str, messages: &[String]) -> Result<String, String> {
    let inner = prefix
        .trim()
        .strip_prefix('[')
        .and_then(|p| p.strip_suffix(']'))
        .ok_or("the prefix is not a JSON array")?
        .trim();
    let mut body = String::with_capacity(
        head.len() + inner.len() + messages.iter().map(|m| m.len() + 1).sum::<usize>() + 16,
    );
    body.push('{');
    body.push_str(head);
    body.push_str(",\"messages\":[");
    body.push_str(inner);
    for (n, message) in messages.iter().enumerate() {
        if n > 0 || !inner.is_empty() {
            body.push(',');
        }
        body.push_str(message);
    }
    body.push_str("]}");
    Ok(body)
}

/// The body of the request event at `index` of `events`, rebuilt from the
/// log as it was sent.
pub fn body(events: &[Event], index: usize, prefix_file: &str) -> Result<String, String> {
    let event = events.get(index).ok_or("no such event")?;
    let Kind::Request {
        purpose,
        prefix,
        head,
        ..
    } = &event.kind
    else {
        return Err(format!("event {} is not a request", event.seq));
    };
    match purpose {
        Purpose::Title => Ok(format!("{{{head}}}")),
        Purpose::Turn => {
            let before = events.get(..index).unwrap_or_default();
            let prefix = prefix_text(before, prefix_file, *prefix)
                .ok_or_else(|| format!("request {} names no prefix {prefix}", event.seq))?;
            turn_body(head, prefix, &messages(before))
        }
    }
}

/// The prompt's tokens as DESIGN.md §14 estimates them: what the last
/// turn request's response reported for its prompt and completion, plus
/// a quarter of a token a byte for what the body has grown by since; and
/// never less than a quarter of the whole body, which is the estimate
/// before any report.
pub fn estimate(events: &[Event], bytes: u64) -> u64 {
    let whole = bytes.div_ceil(4);
    let mut last: Option<(u64, u64)> = None;
    let mut sizes: Vec<(u64, u64)> = Vec::new();
    for event in events {
        match &event.kind {
            Kind::Request {
                purpose: Purpose::Turn,
                bytes,
                ..
            } => sizes.push((event.seq, *bytes)),
            Kind::Usage {
                request, tokens, ..
            } if tokens.prompt > 0 => {
                if let Some((_, sent)) = sizes.iter().rev().find(|(seq, _)| seq == request) {
                    last = Some((tokens.prompt.saturating_add(tokens.completion), *sent));
                }
            }
            _ => {}
        }
    }
    match last {
        Some((tokens, sent)) => {
            whole.max(tokens.saturating_add(bytes.saturating_sub(sent).div_ceil(4)))
        }
        None => whole,
    }
}

/// A completion as a reply carried it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Completion {
    pub content: Option<String>,
    pub reasoning: Option<String>,
    /// `reasoning_details` as the response's exact bytes.
    pub details: Option<String>,
    pub finish: String,
    pub usage: Option<Usage>,
}

/// A reply's `usage`: its token counts, and its cost where it gave one.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
    pub tokens: Tokens,
    pub cost: Option<u64>,
}

/// Why a request produced no completion, and what follows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Failure {
    /// Not run, and not worth asking again unchanged: 401, 402 and the
    /// other refusals. The turn stops and says why.
    Stop {
        status: Option<u16>,
        message: String,
    },
    /// 429: not run; asked again after `wait`, or a backoff of td-agent's
    /// own when the provider names none.
    RateLimited {
        message: String,
        wait: Option<Duration>,
    },
    /// 502, 503, a transport failure or an error inside a 200: the
    /// provider may have generated, and billed, before failing. Shown with
    /// a retry action, never retried by itself.
    Retryable {
        status: Option<u16>,
        message: String,
        usage: Option<Usage>,
    },
}

impl Failure {
    /// The turn's outcome for it.
    pub fn outcome(&self) -> String {
        let said = |status: &Option<u16>, message: &str| match status {
            Some(status) => format!("error {status}: {message}"),
            None => format!("error: {message}"),
        };
        match self {
            Self::Stop { status, message } => said(status, message),
            Self::RateLimited { message, .. } => said(&Some(429), message),
            Self::Retryable {
                status, message, ..
            } => said(status, message),
        }
    }
}

/// A provider's message, one bounded line of text.
fn tidy(message: &str) -> String {
    let mut out: String = message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_MESSAGE)
        .collect();
    if message.chars().count() > MAX_MESSAGE {
        out.push('\u{2026}');
    }
    let out = out.trim().to_string();
    if out.is_empty() {
        "no message".into()
    } else {
        out
    }
}

/// An error object's code and message: `{"code":402,"message":"..."}`.
fn error_object(error: &Json) -> (Option<u16>, String) {
    let code = error
        .get("code")
        .and_then(Json::as_u64)
        .and_then(|c| u16::try_from(c).ok());
    let message = error
        .get("message")
        .and_then(Json::as_str)
        .map(tidy)
        .unwrap_or_else(|| tidy(&error.to_string()));
    // A provider's raw error, where OpenRouter passes one on, says more.
    let raw = error
        .get("metadata")
        .and_then(|m| m.get("raw"))
        .and_then(Json::as_str)
        .map(|raw| format!("{message} ({})", tidy(raw)));
    (code, raw.unwrap_or(message))
}

/// A reply's `usage`, when it says what the request cost: its `cost`, or
/// at least its prompt and completion counts to compute one from. One
/// that says neither is no usage, so the request is charged its whole
/// reservation rather than nothing.
fn usage(value: &Json) -> Option<Usage> {
    let usage = value.get("usage").filter(|u| !u.is_null())?;
    let count = |path: &[&str]| usage.get_path(path).and_then(Json::as_u64).unwrap_or(0);
    let cost = match usage.get("cost") {
        Some(Json::Num(text)) => cost::parse(text, false),
        _ => None,
    };
    let counted = |name: &str| usage.get(name).and_then(Json::as_u64).is_some();
    if cost.is_none() && !(counted("prompt_tokens") && counted("completion_tokens")) {
        return None;
    }
    Some(Usage {
        tokens: Tokens {
            prompt: count(&["prompt_tokens"]),
            completion: count(&["completion_tokens"]),
            cached: count(&["prompt_tokens_details", "cached_tokens"]),
            cache_write: count(&["prompt_tokens_details", "cache_write_tokens"]),
            reasoning: count(&["completion_tokens_details", "reasoning_tokens"]),
        },
        cost,
    })
}

/// Whether a fetch failure happened before the request left this machine:
/// the service could not be reached, or refused the request as it stood.
/// The service also refuses a response past a bound (its body over the
/// request's limit, its headers, its memory), after the origin has run
/// and may have billed the request.
fn unsent(error: &td_fetch::Error) -> bool {
    match error {
        td_fetch::Error::Refused(m) => {
            !(m.starts_with("response ") || m.starts_with("the exchange with the origin"))
        }
        td_fetch::Error::Malformed(_) => true,
        td_fetch::Error::Io(m) => m.starts_with("no td-fetch socket") || m.starts_with("connect:"),
        td_fetch::Error::Transport(_) => false,
    }
}

/// What a request's fetch came to.
pub fn classify(
    result: Result<td_fetch::Response, td_fetch::Error>,
) -> Result<Completion, Failure> {
    let response = match result {
        Ok(response) => response,
        Err(e) if unsent(&e) => {
            return Err(Failure::Stop {
                status: None,
                message: e.to_string(),
            })
        }
        Err(e) => {
            return Err(Failure::Retryable {
                status: None,
                message: e.to_string(),
                usage: None,
            })
        }
    };
    let value = crate::json::parse_slice(&response.body);
    if response.status != 200 {
        // The status decides; the body's error object only says why.
        let message = match value.as_ref().ok().and_then(|v| v.get("error")) {
            Some(error) => error_object(error).1,
            None => tidy(&String::from_utf8_lossy(
                response
                    .body
                    .get(..MAX_MESSAGE.min(response.body.len()))
                    .unwrap_or_default(),
            )),
        };
        return Err(by_status(
            response.status,
            message,
            response.header("retry-after"),
        ));
    }
    let value = value.map_err(|e| Failure::Retryable {
        status: Some(200),
        message: format!("the reply is not JSON: {e}"),
        usage: None,
    })?;
    // An error object inside a 200 (DESIGN.md §5). Whatever code it
    // names, the status said the request was taken, so it may have run
    // and been billed: never asked again by itself, and charged as such.
    if let Some(error) = value.get("error").filter(|e| !e.is_null()) {
        let (code, message) = error_object(error);
        return Err(Failure::Retryable {
            status: code.or(Some(200)),
            message,
            usage: usage(&value),
        });
    }
    let choice = value
        .get("choices")
        .and_then(|c| c.index(0))
        .ok_or_else(|| Failure::Retryable {
            status: Some(200),
            message: "a reply with no choices".into(),
            usage: usage(&value),
        })?;
    let finish = match choice.get("finish_reason") {
        Some(Json::Str(reason)) => reason.clone(),
        _ => "unknown".into(),
    };
    if let Some(error) = choice.get("error").filter(|e| !e.is_null()) {
        let (code, message) = error_object(error);
        return Err(Failure::Retryable {
            status: code.or(Some(200)),
            message,
            usage: usage(&value),
        });
    }
    if finish == "error" {
        return Err(Failure::Retryable {
            status: Some(200),
            message: "the provider ended the completion with an error".into(),
            usage: usage(&value),
        });
    }
    let message = choice.get("message").ok_or_else(|| Failure::Retryable {
        status: Some(200),
        message: "a choice with no message".into(),
        usage: usage(&value),
    })?;
    let text = |name: &str| -> Result<Option<String>, Failure> {
        match message.get(name) {
            None | Some(Json::Null) => Ok(None),
            Some(Json::Str(text)) => Ok(Some(text.clone())),
            Some(_) => Err(Failure::Retryable {
                status: Some(200),
                message: format!("the message's {name} is not text"),
                usage: usage(&value),
            }),
        }
    };
    let details = match message.get("reasoning_details") {
        None | Some(Json::Null) => None,
        Some(Json::Arr(_)) => {
            let range = span::find(
                &response.body,
                &[
                    Step::Key("choices"),
                    Step::Index(0),
                    Step::Key("message"),
                    Step::Key("reasoning_details"),
                ],
            )
            .ok_or_else(|| Failure::Retryable {
                status: Some(200),
                message: "reasoning_details could not be found in the reply's bytes".into(),
                usage: usage(&value),
            })?;
            let bytes = response.body.get(range).unwrap_or_default();
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
        Some(_) => {
            return Err(Failure::Retryable {
                status: Some(200),
                message: "reasoning_details is not a list".into(),
                usage: usage(&value),
            })
        }
    };
    Ok(Completion {
        content: text("content")?,
        reasoning: text("reasoning")?,
        details,
        finish,
        usage: usage(&value),
    })
}

/// A failure by its HTTP status (DESIGN.md §5).
fn by_status(status: u16, message: String, retry_after: Option<&str>) -> Failure {
    match status {
        429 => Failure::RateLimited {
            message,
            wait: retry_after
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs),
        },
        // A provider timing out may have run: as 5xx.
        408 => Failure::Retryable {
            status: Some(status),
            message,
            usage: None,
        },
        400..=499 => Failure::Stop {
            status: Some(status),
            message,
        },
        500..=599 => Failure::Retryable {
            status: Some(status),
            message,
            usage: None,
        },
        // A POST is never redirected for us; anything else is a refusal.
        _ => Failure::Stop {
            status: Some(status),
            message,
        },
    }
}

/// How long to wait before rate-limited retry `attempt` (from 0): the
/// provider's `Retry-After` when it gave one, else 1, 2 and 4 seconds;
/// `None` when the provider asks for longer than td-agent waits.
pub fn backoff(attempt: u32, asked: Option<Duration>) -> Option<Duration> {
    let wait = asked.unwrap_or_else(|| Duration::from_secs(1u64 << attempt.min(6)));
    (wait <= MAX_WAIT).then_some(wait)
}

/// A title's text from a title reply: its first line, quotes and closing
/// punctuation trimmed, bounded as every title is.
pub fn title(content: &str) -> Option<String> {
    let line = content.trim().lines().next()?;
    let line = line
        .trim()
        .trim_matches(['"', '\'', '`', '*', '#'])
        .trim_end_matches(['.', '!'])
        .trim();
    let title = crate::store::title(line);
    (!title.is_empty()).then_some(title)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use crate::store::{Basis, Effect};

    fn event(seq: u64, kind: Kind) -> Event {
        Event { seq, time: 0, kind }
    }

    fn response(status: u16, body: &str, headers: &[(&str, &str)]) -> td_fetch::Response {
        td_fetch::Response {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn the_head_carries_the_parameters_the_design_names() {
        let client = Client::default();
        let params = Params {
            model: "anthropic/claude-sonnet-5.5",
            max_tokens: 16384,
            effort: Some("medium"),
            client: &client,
        };
        assert_eq!(
            head(&params),
            "\"model\":\"anthropic/claude-sonnet-5.5\",\"max_tokens\":16384,\
             \"reasoning\":{\"effort\":\"medium\"},\
             \"provider\":{\"require_parameters\":true,\"data_collection\":\"deny\"},\
             \"cache_control\":{\"type\":\"ephemeral\"}"
        );
        let allow = Client {
            allow_data_collection: true,
            ..Client::default()
        };
        let params = Params {
            model: "google/gemini-3-pro",
            max_tokens: 100,
            effort: None,
            client: &allow,
        };
        assert_eq!(
            head(&params),
            "\"model\":\"google/gemini-3-pro\",\"max_tokens\":100,\
             \"provider\":{\"require_parameters\":true,\"data_collection\":\"allow\"}"
        );
        let names: Vec<&str> = headers("sk-x").iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            [
                "authorization",
                "content-type",
                "http-referer",
                "x-openrouter-title"
            ]
        );
        assert_eq!(headers("sk-x")[0].1, "Bearer sk-x");
        assert_eq!(headers("sk-x")[3].1, "td-agent");
    }

    #[test]
    fn a_body_is_the_head_the_prefix_and_the_logs_messages() {
        let prefix = crate::prompt::prefix(crate::store::Role::Conversation);
        let details = r#"[ {"type":"reasoning.encrypted","data":"q\/w=="} ]"#;
        let events = vec![
            event(
                1,
                Kind::User {
                    delivery: "d".into(),
                    text: "hi \"you\"".into(),
                },
            ),
            event(
                2,
                Kind::Started {
                    effect: Effect::Turn,
                    of: 1,
                },
            ),
            event(
                3,
                Kind::Request {
                    turn: 2,
                    purpose: Purpose::Turn,
                    prefix: 0,
                    head: "\"model\":\"m\"".into(),
                    bytes: 0,
                    reserved: 0,
                },
            ),
            event(
                4,
                Kind::Assistant {
                    request: 3,
                    content: Some("hello".into()),
                    reasoning: Some("thought".into()),
                    details: Some(details.into()),
                    finish: "stop".into(),
                },
            ),
            // A title request's reply is not part of the conversation.
            event(
                5,
                Kind::Request {
                    turn: 2,
                    purpose: Purpose::Title,
                    prefix: 0,
                    head: "\"model\":\"t\"".into(),
                    bytes: 0,
                    reserved: 0,
                },
            ),
            event(
                6,
                Kind::Title {
                    request: 5,
                    text: "x".into(),
                },
            ),
            event(
                7,
                Kind::User {
                    delivery: "e".into(),
                    text: "again".into(),
                },
            ),
            event(8, Kind::Prefix { text: "[]".into() }),
            event(
                9,
                Kind::Request {
                    turn: 2,
                    purpose: Purpose::Turn,
                    prefix: 8,
                    head: "\"model\":\"m\"".into(),
                    bytes: 0,
                    reserved: 0,
                },
            ),
        ];
        let first = body(&events, 2, &prefix).unwrap();
        let system = prefix.trim_start_matches('[').trim_end_matches(']');
        assert_eq!(
            first,
            format!("{{\"model\":\"m\",\"messages\":[{system},{{\"role\":\"user\",\"content\":\"hi \\\"you\\\"\"}}]}}")
        );
        let second = body(&events, 8, &prefix).unwrap();
        assert_eq!(
            second,
            format!(
                "{{\"model\":\"m\",\"messages\":[\
                 {{\"role\":\"user\",\"content\":\"hi \\\"you\\\"\"}},\
                 {{\"role\":\"assistant\",\"content\":\"hello\",\"reasoning_details\":{details}}},\
                 {{\"role\":\"user\",\"content\":\"again\"}}]}}"
            )
        );
        crate::json::parse(&first).unwrap();
        crate::json::parse(&second).unwrap();
        assert_eq!(body(&events, 4, &prefix).unwrap(), "{\"model\":\"t\"}");
        assert!(body(&events, 0, &prefix).is_err());
        assert_eq!(current_prefix(&events, &prefix), (8, "[]"));
        assert_eq!(current_prefix(&events[..7], &prefix), (0, prefix.as_str()));
    }

    #[test]
    fn a_reply_with_no_text_and_no_reasoning_is_not_sent_back() {
        let events = vec![
            event(
                1,
                Kind::Request {
                    turn: 0,
                    purpose: Purpose::Turn,
                    prefix: 0,
                    head: String::new(),
                    bytes: 0,
                    reserved: 0,
                },
            ),
            event(
                2,
                Kind::Assistant {
                    request: 1,
                    content: None,
                    reasoning: None,
                    details: None,
                    finish: "length".into(),
                },
            ),
            event(
                3,
                Kind::Request {
                    turn: 0,
                    purpose: Purpose::Turn,
                    prefix: 0,
                    head: String::new(),
                    bytes: 0,
                    reserved: 0,
                },
            ),
            event(
                4,
                Kind::Assistant {
                    request: 3,
                    content: None,
                    reasoning: None,
                    details: Some("[]".into()),
                    finish: "stop".into(),
                },
            ),
        ];
        assert_eq!(
            messages(&events),
            ["{\"role\":\"assistant\",\"content\":\"\",\"reasoning_details\":[]}"]
        );
    }

    /// The reasoning details go into the log as the reply's own bytes and
    /// come back out of it into the next body unchanged, whatever their
    /// spacing and escapes.
    #[test]
    fn the_reasoning_splice_is_byte_identical() {
        let details = "[\n  {\"type\": \"reasoning.text\", \"text\": \"a\\u00e9\\/b\", \"signature\": \"c2ln\\n\" ,\"format\":\"anthropic-claude-v1\",\"index\":0},\n  {\"type\":\"reasoning.encrypted\",\"data\":\"QUJD\"}\n]";
        let reply = format!(
            "{{\"id\":\"gen-1\",\"choices\":[{{\"index\":0,\"finish_reason\":\"stop\",\"message\":{{\"role\":\"assistant\",\"content\":\"ok\",\"reasoning\":\"thinking\",\"reasoning_details\":{details}}}}}],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":2,\"cost\":1.5e-5}}}}"
        );
        let completion = classify(Ok(response(200, &reply, &[]))).unwrap();
        assert_eq!(completion.details.as_deref(), Some(details));
        // Through the log's own encoding and back.
        let logged = Event {
            seq: 2,
            time: 0,
            kind: Kind::Assistant {
                request: 1,
                content: completion.content.clone(),
                reasoning: completion.reasoning.clone(),
                details: completion.details.clone(),
                finish: completion.finish.clone(),
            },
        };
        let line = logged.to_json().to_string();
        let back = Event::from_json(&crate::json::parse(&line).unwrap()).unwrap();
        let request = event(
            1,
            Kind::Request {
                turn: 0,
                purpose: Purpose::Turn,
                prefix: 0,
                head: String::new(),
                bytes: 0,
                reserved: 0,
            },
        );
        let sent = messages(&[request, back]);
        assert_eq!(
            sent,
            [format!(
                "{{\"role\":\"assistant\",\"content\":\"ok\",\"reasoning_details\":{details}}}"
            )]
        );
        assert_eq!(completion.reasoning.as_deref(), Some("thinking"));
        assert_eq!(
            completion.usage,
            Some(Usage {
                tokens: Tokens {
                    prompt: 10,
                    completion: 2,
                    ..Tokens::default()
                },
                cost: Some(15_000_000),
            })
        );
    }

    #[test]
    fn each_error_path_is_classified() {
        let error = |code: u16, message: &str| {
            format!("{{\"error\":{{\"code\":{code},\"message\":\"{message}\"}}}}")
        };
        // 401 and 402 stop the turn.
        for status in [401, 402] {
            let failure = classify(Ok(response(status, &error(status, "no"), &[]))).unwrap_err();
            assert_eq!(
                failure,
                Failure::Stop {
                    status: Some(status),
                    message: "no".into()
                }
            );
            assert_eq!(failure.outcome(), format!("error {status}: no"));
        }
        // 429 with and without Retry-After.
        assert_eq!(
            classify(Ok(response(
                429,
                &error(429, "slow"),
                &[("retry-after", "7")]
            )))
            .unwrap_err(),
            Failure::RateLimited {
                message: "slow".into(),
                wait: Some(Duration::from_secs(7))
            }
        );
        assert_eq!(
            classify(Ok(response(429, "", &[]))).unwrap_err(),
            Failure::RateLimited {
                message: "no message".into(),
                wait: None
            }
        );
        // 502 and 503 may have run: shown with a retry action.
        for status in [500, 502, 503] {
            assert!(matches!(
                classify(Ok(response(status, "<html>bad gateway</html>", &[]))),
                Err(Failure::Retryable { status: Some(s), ref message, .. })
                    if s == status && message == "<html>bad gateway</html>"
            ));
        }
        // An error object inside a 200.
        assert!(matches!(
            classify(Ok(response(200, &error(502, "upstream"), &[]))),
            Err(Failure::Retryable { status: Some(502), ref message, .. }) if message == "upstream"
        ));
        // Whatever code it names: the 200 took the request, which may
        // have run, so it is not retried by itself nor charged nothing.
        for code in [402, 429, 408] {
            assert!(matches!(
                classify(Ok(response(200, &error(code, "x"), &[]))),
                Err(Failure::Retryable { status: Some(s), .. }) if s == code
            ));
        }
        // A usage that says no cost and not both counts is none, so the
        // request is charged its reservation, not zero.
        for partial in [
            r#"{}"#,
            r#"{"prompt_tokens":5}"#,
            r#"{"completion_tokens":5,"cost":"0.1"}"#,
        ] {
            let reply = format!(
                r#"{{"choices":[{{"finish_reason":"stop","message":{{"content":"hi"}}}}],"usage":{partial}}}"#
            );
            assert_eq!(
                classify(Ok(response(200, &reply, &[]))).unwrap().usage,
                None,
                "{partial}"
            );
        }
        // A choice that ended in error, with usage to charge.
        let choice_error = r#"{"choices":[{"finish_reason":"error","error":{"code":500,"message":"died","metadata":{"raw":"provider said x"}},"message":{"content":""}}],"usage":{"prompt_tokens":5,"completion_tokens":1,"cost":0.001}}"#;
        assert!(matches!(
            classify(Ok(response(200, choice_error, &[]))),
            Err(Failure::Retryable { status: Some(500), ref message, usage: Some(Usage { cost: Some(1_000_000_000), .. }) })
                if message == "died (provider said x)"
        ));
        for broken in [
            "not json",
            "{}",
            r#"{"choices":[{}]}"#,
            r#"{"choices":[{"message":{"content":1}}]}"#,
        ] {
            assert!(
                matches!(
                    classify(Ok(response(200, broken, &[]))),
                    Err(Failure::Retryable { .. })
                ),
                "{broken}"
            );
        }
        // Other refusals stop; the service's own refusals were never sent.
        assert!(matches!(
            classify(Ok(response(400, &error(400, "bad"), &[]))),
            Err(Failure::Stop {
                status: Some(400),
                ..
            })
        ));
        assert!(matches!(
            classify(Err(td_fetch::Error::Io("no td-fetch socket".into()))),
            Err(Failure::Stop { status: None, .. })
        ));
        assert!(matches!(
            classify(Err(td_fetch::Error::Refused("loopback".into()))),
            Err(Failure::Stop { status: None, .. })
        ));
        // A response the service refused for its size came from a run.
        for refused in [
            "response over 524288 bytes",
            "response over 64 headers",
            "the exchange with the origin passed its 64 MiB memory bound",
        ] {
            assert!(
                matches!(
                    classify(Err(td_fetch::Error::Refused(refused.into()))),
                    Err(Failure::Retryable { status: None, .. })
                ),
                "{refused}"
            );
        }
        // A transport failure after sending may have run.
        assert!(matches!(
            classify(Err(td_fetch::Error::Transport("reset".into()))),
            Err(Failure::Retryable { status: None, .. })
        ));
        assert!(matches!(
            classify(Err(td_fetch::Error::Io("read: timed out".into()))),
            Err(Failure::Retryable { status: None, .. })
        ));
        let long = "x".repeat(2000);
        let Err(Failure::Stop { message, .. }) =
            classify(Ok(response(400, &error(400, &long), &[])))
        else {
            panic!("not a stop")
        };
        assert_eq!(message.chars().count(), MAX_MESSAGE + 1);
    }

    #[test]
    fn a_completion_reads_its_text_finish_and_usage() {
        let reply = r#"{"choices":[{"finish_reason":"length","message":{"role":"assistant","content":null,"reasoning":null}}],
            "usage":{"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":80,"cache_write_tokens":10},
                     "completion_tokens_details":{"reasoning_tokens":40}}}"#;
        let completion = classify(Ok(response(200, reply, &[]))).unwrap();
        assert_eq!(completion.content, None);
        assert_eq!(completion.details, None);
        assert_eq!(completion.finish, "length");
        assert_eq!(
            completion.usage,
            Some(Usage {
                tokens: Tokens {
                    prompt: 100,
                    completion: 50,
                    cached: 80,
                    cache_write: 10,
                    reasoning: 40
                },
                cost: None
            })
        );
    }

    #[test]
    fn rate_limited_retries_back_off_within_a_bound() {
        assert_eq!(backoff(0, None), Some(Duration::from_secs(1)));
        assert_eq!(backoff(1, None), Some(Duration::from_secs(2)));
        assert_eq!(backoff(2, None), Some(Duration::from_secs(4)));
        assert_eq!(
            backoff(0, Some(Duration::from_secs(30))),
            Some(Duration::from_secs(30))
        );
        assert_eq!(backoff(0, Some(Duration::ZERO)), Some(Duration::ZERO));
        assert_eq!(backoff(0, Some(Duration::from_secs(61))), None);
    }

    #[test]
    fn the_prompt_estimate_starts_from_the_last_report() {
        let events = vec![
            event(
                1,
                Kind::Request {
                    turn: 0,
                    purpose: Purpose::Turn,
                    prefix: 0,
                    head: String::new(),
                    bytes: 4000,
                    reserved: 0,
                },
            ),
            event(
                2,
                Kind::Usage {
                    request: 1,
                    tokens: Tokens {
                        prompt: 2000,
                        completion: 100,
                        ..Tokens::default()
                    },
                    cost: 0,
                    basis: Basis::Reported,
                },
            ),
        ];
        assert_eq!(estimate(&[], 4001), 1001);
        // 2,100 reported, and 1,000 bytes more since: 250 tokens.
        assert_eq!(estimate(&events, 5000), 2350);
        assert_eq!(estimate(&events, 40_000), 2100 + 9000);
        // Never under a quarter of the body.
        let mut sparse = events.clone();
        if let Kind::Usage { tokens, .. } = &mut sparse[1].kind {
            tokens.prompt = 500;
            tokens.completion = 0;
        }
        assert_eq!(estimate(&sparse, 5000), 1250);
    }

    #[test]
    fn a_title_is_one_trimmed_line() {
        assert_eq!(
            title("\"Fixing the build.\"\nmore").as_deref(),
            Some("Fixing the build")
        );
        assert_eq!(title("  # Plans  ").as_deref(), Some("Plans"));
        assert_eq!(title(""), None);
        assert_eq!(title("\"\""), None);
        let head = title_head(&Client::default(), &"é".repeat(3000), "reply");
        let value = crate::json::parse(&format!("{{{head}}}")).unwrap();
        let quoted = value.get_path(&["messages"]).unwrap().index(1).unwrap();
        let text = quoted.get("content").unwrap().as_str().unwrap();
        assert!(text.len() < TITLE_QUOTE + 100, "{}", text.len());
        assert_eq!(
            value.get("model").unwrap().as_str(),
            Some(crate::config::DEFAULT_TITLE_MODEL)
        );
    }
}
