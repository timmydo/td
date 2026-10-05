//! What the window process and a conversation process say to each other,
//! one JSON object per frame (`frame`), over their socketpair (DESIGN.md
//! §2). The API key crosses here, in the `Setup` the window sends each
//! conversation process first, and never through argv, the environment
//! or a file; `Debug` never shows it.

use crate::config::Client;
use crate::key::Secret;
use crate::store::{Event, Id, Role};
use td_json::Json;

/// The longest message a human sends in one go. JSON escaping can make it
/// six times longer on the wire, which `frame::MAX_FRAME` holds.
pub const MAX_TEXT: usize = 128 * 1024;
/// A delivery id's length: 16 bytes in hexadecimal.
pub const DELIVERY_LEN: usize = 32;

/// From the window to a conversation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Down {
    /// What the model client needs, first on every socketpair: the key,
    /// or why there is none, and the configuration's settings.
    Setup {
        key: Result<Secret, String>,
        client: Client,
    },
    /// The human's message, with an id the conversation logs once.
    User { delivery: String, text: String },
    /// The answer to a `Reserve` of the same id: granted, or why not.
    Reservation { id: u64, refusal: Option<String> },
    /// Ask again for the last turn, which failed in a way that may pass.
    Retry,
    /// Interrupt the turn under way: its stream's connection is closed
    /// (DESIGN.md §5). Between turns it means nothing.
    Interrupt,
    /// A message from another conversation, which the receiver logs once
    /// by its delivery id and takes between turns (DESIGN.md §3).
    Message {
        delivery: String,
        from: Id,
        role: Role,
        text: String,
        status: Option<String>,
    },
    /// The answer to a `Send` of the same id: queued, or why not.
    Sent { id: u64, refusal: Option<String> },
    /// The answer to a `Query` of the same id: each conversation's state
    /// as the window knows it.
    States { id: u64, states: Vec<(Id, String)> },
    /// The human paused or resumed the conversation.
    Pause { paused: bool },
    /// The human cleared the todo list.
    ClearTodo,
    /// The human chose the conversation's model and reasoning effort,
    /// whole; none is the configuration's.
    Choose {
        model: Option<String>,
        effort: Option<String>,
    },
    /// The human's decision on the card the conversation asked for call
    /// `call` (its `ToolCall`'s sequence number).
    Decision { call: u64, allow: bool },
    /// The answer to a `Fetch` of `remote`: the store fetched and each
    /// base resolved, or why not.
    Fetched {
        remote: String,
        result: Result<Fetched, String>,
    },
}

/// A fetched store, for a repository workspace's preparation (DESIGN.md
/// §7): the human's identity its commits carry, and each base asked for
/// resolved to its commit, in the order asked.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Fetched {
    pub identity: crate::repo::Identity,
    pub ids: Vec<String>,
}

/// From a conversation to the window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Up {
    /// The conversation is open: its role and title, what opening it
    /// found (a torn line dropped, effects interrupted), and its prefix
    /// file's text, none when that is past `MAX_TEXT`. Escaped, that
    /// takes at most six times `MAX_TEXT` of the frame, which leaves the
    /// rest, a quarter, for the title and interrupted ids. Every event of
    /// its log follows, in order, then each new one as it is appended.
    Hello {
        title: String,
        torn: Option<u64>,
        interrupted: Vec<u64>,
        paused: bool,
        prefix: Option<String>,
    },
    Event(Event),
    /// The message with this delivery id is logged and synced.
    Delivered {
        delivery: String,
    },
    Title {
        title: String,
    },
    /// A message the conversation refused, and why.
    Refused {
        delivery: String,
        reason: String,
    },
    /// Reserve `amount` pico-credits against the day's limit, which the
    /// window holds, for request `id` of this process (DESIGN.md §5).
    Reserve {
        id: u64,
        amount: u64,
    },
    /// Request `id` cost `amount`, in place of what it reserved.
    Spent {
        id: u64,
        amount: u64,
    },
    /// What a streamed reply to request `request` (its sequence number)
    /// has brought since the last: reasoning, then text, for the window to
    /// draw as it arrives. Not logged: the reply is, whole, once it ends.
    Delta {
        request: u64,
        reasoning: String,
        content: String,
    },
    /// `send_message`, the human having allowed it: queue `text` for
    /// conversation `to`. The window answers with `Sent`.
    Send {
        id: u64,
        to: Id,
        text: String,
    },
    /// `conversations`: what state is each conversation in? The window
    /// answers with `States`.
    Query {
        id: u64,
    },
    /// A card for the human (DESIGN.md §11): may call `call` (its
    /// `ToolCall`'s sequence number) run, as `title` and `details` say?
    /// The window answers with `Decision`.
    Ask {
        call: u64,
        title: String,
        details: Vec<String>,
    },
    /// The card for `call` is no longer asked: the turn was interrupted,
    /// or the window closed, before the human decided it.
    Withdraw {
        call: u64,
    },
    /// A repository workspace's store for `remote` is wanted: the window
    /// fetches it and resolves `bases` there, and answers with `Fetched`.
    Fetch {
        remote: String,
        bases: Vec<String>,
    },
    /// The conversation is done preparing `remote`'s repository, ready or
    /// not: until then the window keeps its process (DESIGN.md §7).
    Prepared {
        remote: String,
    },
}

fn typed(kind: &str, mut pairs: Vec<(String, Json)>) -> Vec<u8> {
    pairs.insert(0, ("type".into(), Json::Str(kind.into())));
    Json::Obj(pairs).to_string().into_bytes()
}

fn string(value: &Json, name: &str) -> Result<String, String> {
    value
        .get(name)
        .and_then(Json::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("no {name}"))
}

/// Whether `delivery` is a delivery id as the window makes them.
pub fn delivery_ok(delivery: &str) -> bool {
    delivery.len() == DELIVERY_LEN
        && delivery
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// An optional text as a frame carries it: the text, or null.
fn optional(text: &Option<String>) -> Json {
    text.clone().map_or(Json::Null, Json::Str)
}

/// An optional text member, which a frame must carry, as null when none.
fn maybe(value: &Json, name: &str) -> Result<Option<String>, String> {
    match value.get(name) {
        Some(Json::Null) => Ok(None),
        Some(Json::Str(text)) => Ok(Some(text.clone())),
        _ => Err(format!("no {name}")),
    }
}

/// A member that is a list of texts.
fn strings(value: &Json, name: &str) -> Result<Vec<String>, String> {
    let strings: Vec<String> = value
        .get(name)
        .and_then(Json::as_arr)
        .ok_or_else(|| format!("no {name}"))?
        .iter()
        .take(MAX_STRINGS + 1)
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{name} holds something other than text"))
        })
        .collect::<Result<_, _>>()?;
    if strings.len() > MAX_STRINGS {
        return Err(format!("{name} holds more than {MAX_STRINGS} items"));
    }
    Ok(strings)
}

/// The most bases a `Fetch` asks and commits a `Fetched` answers, as
/// many as a repository workspace has worktrees.
const MAX_STRINGS: usize = crate::workspace::MAX_ENTRIES;

fn number(value: &Json, name: &str) -> Result<u64, String> {
    value
        .get(name)
        .and_then(Json::as_u64)
        .ok_or_else(|| format!("no {name}"))
}

impl Down {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Setup { key, client } => {
                let key = match key {
                    Ok(key) => ("key".to_string(), Json::Str(key.expose().into())),
                    Err(why) => ("no_key".to_string(), Json::Str(why.clone())),
                };
                typed("setup", vec![key, ("client".into(), client.to_json())])
            }
            Self::User { delivery, text } => typed(
                "user",
                vec![
                    ("delivery".into(), Json::Str(delivery.clone())),
                    ("text".into(), Json::Str(text.clone())),
                ],
            ),
            Self::Reservation { id, refusal } => typed(
                "reservation",
                vec![
                    ("id".into(), Json::from(*id)),
                    (
                        "refusal".into(),
                        refusal.clone().map_or(Json::Null, Json::Str),
                    ),
                ],
            ),
            Self::Retry => typed("retry", Vec::new()),
            Self::Interrupt => typed("interrupt", Vec::new()),
            Self::Message {
                delivery,
                from,
                role,
                text,
                status,
            } => typed(
                "message",
                vec![
                    ("delivery".into(), Json::Str(delivery.clone())),
                    ("from".into(), Json::Str(from.to_string())),
                    ("role".into(), Json::Str(role.word().into())),
                    ("text".into(), Json::Str(text.clone())),
                    ("status".into(), optional(status)),
                ],
            ),
            Self::Sent { id, refusal } => typed(
                "sent",
                vec![
                    ("id".into(), Json::from(*id)),
                    ("refusal".into(), optional(refusal)),
                ],
            ),
            Self::States { id, states } => typed(
                "states",
                vec![
                    ("id".into(), Json::from(*id)),
                    (
                        "states".into(),
                        Json::Arr(
                            states
                                .iter()
                                .map(|(id, state)| {
                                    Json::Arr(vec![
                                        Json::Str(id.to_string()),
                                        Json::Str(state.clone()),
                                    ])
                                })
                                .collect(),
                        ),
                    ),
                ],
            ),
            Self::Pause { paused } => typed("pause", vec![("paused".into(), Json::Bool(*paused))]),
            Self::ClearTodo => typed("clear_todo", Vec::new()),
            Self::Choose { model, effort } => typed(
                "choose",
                vec![
                    ("model".into(), optional(model)),
                    ("effort".into(), optional(effort)),
                ],
            ),
            Self::Decision { call, allow } => typed(
                "decision",
                vec![
                    ("call".into(), Json::from(*call)),
                    ("allow".into(), Json::Bool(*allow)),
                ],
            ),
            Self::Fetched { remote, result } => {
                let mut pairs = vec![("remote".into(), Json::Str(remote.clone()))];
                match result {
                    Ok(fetched) => pairs.extend([
                        ("error".into(), Json::Null),
                        ("name".into(), optional(&fetched.identity.name)),
                        ("email".into(), optional(&fetched.identity.email)),
                        (
                            "ids".into(),
                            Json::Arr(fetched.ids.iter().cloned().map(Json::Str).collect()),
                        ),
                    ]),
                    Err(why) => pairs.push(("error".into(), Json::Str(why.clone()))),
                }
                typed("fetched", pairs)
            }
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let value = td_json::parse_slice(bytes).map_err(|e| e.to_string())?;
        match value.get("type").and_then(Json::as_str) {
            Some("setup") => {
                let key = match (value.get("key"), value.get("no_key")) {
                    (Some(Json::Str(key)), None) if crate::key::plausible(key) => {
                        Ok(Secret::new(key.clone()))
                    }
                    (Some(_), None) => return Err("a malformed key".into()),
                    (None, Some(Json::Str(why))) => Err(why.clone()),
                    _ => return Err("a setup with neither a key nor why not".into()),
                };
                let client =
                    Client::from_json(value.get("client").ok_or("a setup with no client")?)?;
                Ok(Self::Setup { key, client })
            }
            Some("reservation") => Ok(Self::Reservation {
                id: number(&value, "id")?,
                refusal: match value.get("refusal") {
                    Some(Json::Null) => None,
                    Some(Json::Str(why)) => Some(why.clone()),
                    _ => return Err("a reservation with no refusal field".into()),
                },
            }),
            Some("retry") => Ok(Self::Retry),
            Some("interrupt") => Ok(Self::Interrupt),
            Some("message") => {
                let delivery = string(&value, "delivery")?;
                if !delivery_ok(&delivery) {
                    return Err("a malformed delivery id".into());
                }
                Ok(Self::Message {
                    delivery,
                    from: Id::parse(&string(&value, "from")?).ok_or("a malformed sender")?,
                    role: Role::parse(&string(&value, "role")?).ok_or("a malformed role")?,
                    text: string(&value, "text")?,
                    status: maybe(&value, "status")?,
                })
            }
            Some("sent") => Ok(Self::Sent {
                id: number(&value, "id")?,
                refusal: maybe(&value, "refusal")?,
            }),
            Some("states") => Ok(Self::States {
                id: number(&value, "id")?,
                states: value
                    .get("states")
                    .and_then(Json::as_arr)
                    .ok_or("no states")?
                    .iter()
                    .map(|pair| {
                        let id = pair.index(0).and_then(Json::as_str).and_then(Id::parse);
                        let state = pair.index(1).and_then(Json::as_str);
                        match (id, state) {
                            (Some(id), Some(state)) => Ok((id, state.to_string())),
                            _ => Err("a malformed state"),
                        }
                    })
                    .collect::<Result<_, _>>()?,
            }),
            Some("pause") => Ok(Self::Pause {
                paused: value
                    .get("paused")
                    .and_then(Json::as_bool)
                    .ok_or("no paused")?,
            }),
            Some("clear_todo") => Ok(Self::ClearTodo),
            Some("choose") => {
                let model = maybe(&value, "model")?;
                let effort = maybe(&value, "effort")?;
                if let Some(model) = &model {
                    crate::config::model_id("model", model)?;
                }
                if let Some(effort) = &effort {
                    crate::config::effort(effort)?;
                }
                Ok(Self::Choose { model, effort })
            }
            Some("user") => {
                let delivery = string(&value, "delivery")?;
                if !delivery_ok(&delivery) {
                    return Err("a malformed delivery id".into());
                }
                Ok(Self::User {
                    delivery,
                    text: string(&value, "text")?,
                })
            }
            Some("decision") => Ok(Self::Decision {
                call: number(&value, "call")?,
                allow: value
                    .get("allow")
                    .and_then(Json::as_bool)
                    .ok_or("no allow")?,
            }),
            Some("fetched") => Ok(Self::Fetched {
                remote: string(&value, "remote")?,
                result: match maybe(&value, "error")? {
                    Some(why) => Err(why),
                    None => Ok(Fetched {
                        identity: crate::repo::Identity {
                            name: maybe(&value, "name")?,
                            email: maybe(&value, "email")?,
                        },
                        ids: strings(&value, "ids")?,
                    }),
                },
            }),
            other => Err(format!("unknown message {other:?}")),
        }
    }
}

impl Up {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Hello {
                title,
                torn,
                interrupted,
                paused,
                prefix,
            } => typed(
                "hello",
                vec![
                    ("title".into(), Json::Str(title.clone())),
                    ("torn".into(), torn.map_or(Json::Null, Json::from)),
                    (
                        "interrupted".into(),
                        Json::Arr(interrupted.iter().map(|s| Json::from(*s)).collect()),
                    ),
                    ("paused".into(), Json::Bool(*paused)),
                    (
                        "prefix".into(),
                        prefix.as_ref().map_or(Json::Null, |p| Json::Str(p.clone())),
                    ),
                ],
            ),
            Self::Event(event) => typed("event", vec![("event".into(), event.to_json())]),
            Self::Delivered { delivery } => typed(
                "delivered",
                vec![("delivery".into(), Json::Str(delivery.clone()))],
            ),
            Self::Title { title } => {
                typed("title", vec![("title".into(), Json::Str(title.clone()))])
            }
            Self::Refused { delivery, reason } => typed(
                "refused",
                vec![
                    ("delivery".into(), Json::Str(delivery.clone())),
                    ("reason".into(), Json::Str(reason.clone())),
                ],
            ),
            Self::Reserve { id, amount } => typed(
                "reserve",
                vec![
                    ("id".into(), Json::from(*id)),
                    ("amount".into(), Json::from(*amount)),
                ],
            ),
            Self::Spent { id, amount } => typed(
                "spent",
                vec![
                    ("id".into(), Json::from(*id)),
                    ("amount".into(), Json::from(*amount)),
                ],
            ),
            Self::Delta {
                request,
                reasoning,
                content,
            } => typed(
                "delta",
                vec![
                    ("request".into(), Json::from(*request)),
                    ("reasoning".into(), Json::Str(reasoning.clone())),
                    ("content".into(), Json::Str(content.clone())),
                ],
            ),
            Self::Send { id, to, text } => typed(
                "send",
                vec![
                    ("id".into(), Json::from(*id)),
                    ("to".into(), Json::Str(to.to_string())),
                    ("text".into(), Json::Str(text.clone())),
                ],
            ),
            Self::Query { id } => typed("query", vec![("id".into(), Json::from(*id))]),
            Self::Prepared { remote } => typed(
                "prepared",
                vec![("remote".into(), Json::Str(remote.clone()))],
            ),
            Self::Fetch { remote, bases } => typed(
                "fetch",
                vec![
                    ("remote".into(), Json::Str(remote.clone())),
                    (
                        "bases".into(),
                        Json::Arr(bases.iter().cloned().map(Json::Str).collect()),
                    ),
                ],
            ),
            Self::Ask {
                call,
                title,
                details,
            } => typed(
                "ask",
                vec![
                    ("call".into(), Json::from(*call)),
                    ("title".into(), Json::Str(title.clone())),
                    (
                        "details".into(),
                        Json::Arr(details.iter().cloned().map(Json::Str).collect()),
                    ),
                ],
            ),
            Self::Withdraw { call } => typed("withdraw", vec![("call".into(), Json::from(*call))]),
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let value = td_json::parse_slice(bytes).map_err(|e| e.to_string())?;
        Ok(match value.get("type").and_then(Json::as_str) {
            Some("hello") => Self::Hello {
                title: string(&value, "title")?,
                torn: value.get("torn").and_then(Json::as_u64),
                interrupted: value
                    .get("interrupted")
                    .and_then(Json::as_arr)
                    .ok_or("no interrupted list")?
                    .iter()
                    .map(|v| v.as_u64().ok_or("a malformed interrupted id"))
                    .collect::<Result<_, _>>()?,
                paused: value
                    .get("paused")
                    .and_then(Json::as_bool)
                    .ok_or("no paused")?,
                prefix: match value.get("prefix") {
                    Some(Json::Null) => None,
                    Some(Json::Str(prefix)) => Some(prefix.clone()),
                    _ => return Err("no prefix".into()),
                },
            },
            Some("event") => Self::Event(Event::from_json(
                value.get("event").ok_or("an event message with no event")?,
            )?),
            Some("delivered") => Self::Delivered {
                delivery: string(&value, "delivery")?,
            },
            Some("title") => Self::Title {
                title: string(&value, "title")?,
            },
            Some("refused") => Self::Refused {
                delivery: string(&value, "delivery")?,
                reason: string(&value, "reason")?,
            },
            Some("reserve") => Self::Reserve {
                id: number(&value, "id")?,
                amount: number(&value, "amount")?,
            },
            Some("spent") => Self::Spent {
                id: number(&value, "id")?,
                amount: number(&value, "amount")?,
            },
            Some("delta") => Self::Delta {
                request: number(&value, "request")?,
                reasoning: string(&value, "reasoning")?,
                content: string(&value, "content")?,
            },
            Some("send") => Self::Send {
                id: number(&value, "id")?,
                to: Id::parse(&string(&value, "to")?).ok_or("a malformed receiver")?,
                text: string(&value, "text")?,
            },
            Some("query") => Self::Query {
                id: number(&value, "id")?,
            },
            Some("ask") => Self::Ask {
                call: number(&value, "call")?,
                title: string(&value, "title")?,
                details: value
                    .get("details")
                    .and_then(Json::as_arr)
                    .ok_or("no details")?
                    .iter()
                    .map(|line| {
                        line.as_str()
                            .map(str::to_string)
                            .ok_or("a detail not a string")
                    })
                    .collect::<Result<_, _>>()?,
            },
            Some("withdraw") => Self::Withdraw {
                call: number(&value, "call")?,
            },
            Some("fetch") => Self::Fetch {
                remote: string(&value, "remote")?,
                bases: strings(&value, "bases")?,
            },
            Some("prepared") => Self::Prepared {
                remote: string(&value, "remote")?,
            },
            other => return Err(format!("unknown message {other:?}")),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::store::Kind;

    fn delivery_text() -> String {
        "0123456789abcdef0123456789abcdef".into()
    }

    #[test]
    fn a_fetch_asks_at_most_as_many_bases_as_a_workspace_has_worktrees() {
        let fetch = |n: usize| Up::Fetch {
            remote: "https://github.com/timmydo/td".into(),
            bases: vec!["main".into(); n],
        };
        let most = fetch(MAX_STRINGS);
        assert_eq!(Up::decode(&most.encode()).unwrap(), most);
        let e = Up::decode(&fetch(MAX_STRINGS + 1).encode()).unwrap_err();
        assert!(e.contains("more than 32"), "{e}");
    }

    #[test]
    fn every_message_round_trips_within_the_frame_bound() {
        let delivery = "0123456789abcdef0123456789abcdef".to_string();
        let down = Down::User {
            delivery: delivery.clone(),
            text: "\u{1}".repeat(MAX_TEXT),
        };
        let bytes = down.encode();
        assert!(bytes.len() <= crate::frame::MAX_FRAME, "{}", bytes.len());
        assert_eq!(Down::decode(&bytes).unwrap(), down);
        for up in [
            Up::Hello {
                title: "Orchestrator".into(),
                torn: Some(3),
                interrupted: vec![2, 5],
                paused: false,
                prefix: Some("\u{1}".repeat(MAX_TEXT)),
            },
            Up::Hello {
                title: "t".into(),
                torn: None,
                interrupted: vec![],
                paused: true,
                prefix: None,
            },
            Up::Send {
                id: 4,
                to: Id::parse(&"b".repeat(32)).unwrap(),
                text: "\u{1}".repeat(crate::tools::MAX_MESSAGE),
            },
            Up::Send {
                id: 5,
                to: Id::parse(&"b".repeat(32)).unwrap(),
                text: "hi".into(),
            },
            Up::Query { id: 6 },
            Up::Fetch {
                remote: "https://github.com/timmydo/td".into(),
                bases: vec!["main".into(), "next".into()],
            },
            Up::Prepared {
                remote: "https://github.com/timmydo/td".into(),
            },
            Up::Ask {
                call: 9,
                title: "Run a command?".into(),
                details: vec!["In /w:".into(), "cargo test".into()],
            },
            Up::Withdraw { call: 9 },
            Up::Event(Event {
                seq: 1,
                time: 2,
                kind: Kind::User {
                    delivery: delivery.clone(),
                    text: "hi".into(),
                },
            }),
            Up::Delivered {
                delivery: delivery.clone(),
            },
            Up::Title { title: "x".into() },
            Up::Refused {
                delivery,
                reason: "too long".into(),
            },
            Up::Reserve { id: 1, amount: 2 },
            Up::Spent {
                id: 1,
                amount: u64::MAX,
            },
            Up::Delta {
                request: 9,
                reasoning: "thinking \u{1}".into(),
                content: String::new(),
            },
            // An incomplete reply's flag survives the log's encoding.
            Up::Event(Event {
                seq: 4,
                time: 5,
                kind: Kind::Assistant {
                    request: 3,
                    content: Some("half".into()),
                    reasoning: None,
                    details: None,
                    finish: "unknown".into(),
                    incomplete: true,
                    calls: Vec::new(),
                },
            }),
        ] {
            assert!(up.encode().len() <= crate::frame::MAX_FRAME);
            assert_eq!(Up::decode(&up.encode()).unwrap(), up);
        }
        let other = Id::parse(&"c".repeat(32)).unwrap();
        for down in [
            Down::Message {
                delivery: delivery_text(),
                from: other.clone(),
                role: Role::Conversation,
                text: "\u{1}".repeat(crate::tools::MAX_MESSAGE),
                status: Some("blocked".into()),
            },
            Down::Message {
                delivery: delivery_text(),
                from: other.clone(),
                role: Role::Orchestrator,
                text: "go".into(),
                status: None,
            },
            Down::Sent {
                id: 1,
                refusal: None,
            },
            Down::Sent {
                id: 2,
                refusal: Some("full".into()),
            },
            Down::States {
                id: 3,
                states: vec![(other, "idle".into())],
            },
            Down::Pause { paused: true },
            Down::ClearTodo,
            Down::Decision {
                call: 7,
                allow: true,
            },
            Down::Decision {
                call: 8,
                allow: false,
            },
            Down::Choose {
                model: Some("openai/gpt-6".into()),
                effort: Some("high".into()),
            },
            Down::Choose {
                model: None,
                effort: None,
            },
        ] {
            assert!(down.encode().len() <= crate::frame::MAX_FRAME);
            assert_eq!(Down::decode(&down.encode()).unwrap(), down);
        }
        let client = Client::default();
        for down in [
            Down::Setup {
                key: Ok(Secret::new("sk-or-v1-abc".into())),
                client: client.clone(),
            },
            Down::Setup {
                key: Err("no API key".into()),
                client,
            },
            Down::Reservation {
                id: 3,
                refusal: None,
            },
            Down::Reservation {
                id: 4,
                refusal: Some("max_cost_per_day".into()),
            },
            Down::Retry,
            Down::Interrupt,
            Down::Fetched {
                remote: "https://github.com/timmydo/td".into(),
                result: Ok(Fetched {
                    identity: crate::repo::Identity {
                        name: Some("Human".into()),
                        email: None,
                    },
                    ids: vec!["a".repeat(40)],
                }),
            },
            Down::Fetched {
                remote: "https://github.com/timmydo/td".into(),
                result: Err("the remote has no branch \"main\"".into()),
            },
        ] {
            assert_eq!(Down::decode(&down.encode()).unwrap(), down);
        }
    }

    #[test]
    fn the_key_crosses_only_in_its_frame_and_never_in_debug() {
        let down = Down::Setup {
            key: Ok(Secret::new("sk-or-v1-secret".into())),
            client: Client::default(),
        };
        assert!(!format!("{down:?}").contains("sk-or-v1-secret"));
        let frame = String::from_utf8(down.encode()).unwrap();
        assert!(frame.contains("\"key\":\"sk-or-v1-secret\""));
        // A key that could not go in a header is refused on arrival.
        let bad = frame.replace("sk-or-v1-secret", "sk or");
        assert!(Down::decode(bad.as_bytes()).is_err());
    }

    #[test]
    fn malformed_messages_are_refused() {
        for bad in [
            &b"not json"[..],
            br#"{"type":"user","delivery":"x","text":"t"}"#,
            br#"{"type":"user","text":"t"}"#,
            br#"{"type":"shout"}"#,
            br#"{}"#,
        ] {
            assert!(Down::decode(bad).is_err());
        }
        for bad in [
            &br#"{"type":"hello","title":"t"}"#[..],
            br#"{"type":"event"}"#,
        ] {
            assert!(Up::decode(bad).is_err());
        }
    }
}
