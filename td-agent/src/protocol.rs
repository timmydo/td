//! What the window process and a conversation process say to each other,
//! one JSON object per frame (`frame`), over their socketpair (DESIGN.md
//! §2). There is no key yet: the model client that needs one is a later
//! increment, and it will cross here, never through argv, the
//! environment or a file.

use crate::json::Json;
use crate::store::{Event, Role};

/// The longest message a human sends in one go. JSON escaping can make it
/// six times longer on the wire, which `frame::MAX_FRAME` holds.
pub const MAX_TEXT: usize = 128 * 1024;
/// A delivery id's length: 16 bytes in hexadecimal.
pub const DELIVERY_LEN: usize = 32;

/// From the window to a conversation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Down {
    /// The human's message, with an id the conversation logs once.
    User { delivery: String, text: String },
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

impl Down {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::User { delivery, text } => typed(
                "user",
                vec![
                    ("delivery".into(), Json::Str(delivery.clone())),
                    ("text".into(), Json::Str(text.clone())),
                ],
            ),
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let value = crate::json::parse_slice(bytes).map_err(|e| e.to_string())?;
        match value.get("type").and_then(Json::as_str) {
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
        ] {
            assert_eq!(Up::decode(&up.encode()).unwrap(), up);
        }
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
