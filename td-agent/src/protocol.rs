//! What the window process and a conversation process say to each other,
//! one JSON object per frame (`frame`), over their socketpair (DESIGN.md
//! §2). The API key crosses here, in the `Setup` the window sends each
//! conversation process first, and never through argv, the environment
//! or a file; `Debug` never shows it.

use crate::config::Client;
use crate::json::Json;
use crate::key::Secret;
use crate::store::{Event, Role};

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
}

/// From a conversation to the window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Up {
    /// The conversation is open: its role and title, and what opening it
    /// found (a torn line dropped, effects interrupted). Every event of
    /// its log follows, in order, then each new one as it is appended.
    Hello {
        role: Role,
        title: String,
        torn: Option<u64>,
        interrupted: Vec<u64>,
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
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let value = crate::json::parse_slice(bytes).map_err(|e| e.to_string())?;
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
            other => Err(format!("unknown message {other:?}")),
        }
    }
}

impl Up {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Hello {
                role,
                title,
                torn,
                interrupted,
            } => typed(
                "hello",
                vec![
                    ("role".into(), Json::Str(role.word().into())),
                    ("title".into(), Json::Str(title.clone())),
                    ("torn".into(), torn.map_or(Json::Null, Json::from)),
                    (
                        "interrupted".into(),
                        Json::Arr(interrupted.iter().map(|s| Json::from(*s)).collect()),
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
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let value = crate::json::parse_slice(bytes).map_err(|e| e.to_string())?;
        Ok(match value.get("type").and_then(Json::as_str) {
            Some("hello") => Self::Hello {
                role: value
                    .get("role")
                    .and_then(Json::as_str)
                    .and_then(Role::parse)
                    .ok_or("no role")?,
                title: string(&value, "title")?,
                torn: value.get("torn").and_then(Json::as_u64),
                interrupted: value
                    .get("interrupted")
                    .and_then(Json::as_arr)
                    .ok_or("no interrupted list")?
                    .iter()
                    .map(|v| v.as_u64().ok_or("a malformed interrupted id"))
                    .collect::<Result<_, _>>()?,
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
            other => return Err(format!("unknown message {other:?}")),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::store::Kind;

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
                role: Role::Orchestrator,
                title: "Orchestrator".into(),
                torn: Some(3),
                interrupted: vec![2, 5],
            },
            Up::Hello {
                role: Role::Conversation,
                title: "t".into(),
                torn: None,
                interrupted: vec![],
            },
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
                },
            }),
        ] {
            assert_eq!(Up::decode(&up.encode()).unwrap(), up);
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
