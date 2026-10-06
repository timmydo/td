//! What the window process and a conversation process say to each other,
//! one JSON object per frame (`frame`), over their socketpair (DESIGN.md
//! §2). The API key crosses here, in the `Setup` the window sends each
//! conversation process first, and never through argv, the environment
//! or a file; `Debug` never shows it.

use crate::config::Client;
use crate::key::Secret;
use crate::store::{Event, Id, Role};
use td_json::Json;

/// The longest text of what an "always" answer remembered.
pub const MAX_REMEMBERED: usize = 8 * 1024;

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
        client: Box<Client>,
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
    /// The human asked to compact the conversation (DESIGN.md §14), with
    /// what the summary should keep in particular.
    Compact { focus: Option<String> },
    /// The human killed background process `number` (DESIGN.md §12):
    /// acted on as soon as it is heard, a turn under way or not.
    Kill { number: u64 },
    /// The human's rules file (DESIGN.md §11), whole, or why it could not
    /// be read, numbered so a change is known: second on every
    /// socketpair, after `Setup`, and again on every change, acted on as
    /// soon as it is heard.
    Policy {
        version: u64,
        rules: Result<String, String>,
        /// The configuration's mode, a workspace's own when the file
        /// sets none.
        mode: crate::config::Mode,
    },
    /// Undo, or redo, the step snapshotted at `step` (DESIGN.md §12).
    Restore { step: u64, undo: bool },
    /// The human chose the conversation's model and reasoning effort,
    /// whole; none is the configuration's.
    Choose {
        model: Option<String>,
        effort: Option<String>,
    },
    /// The human's decision on the card the conversation asked for call
    /// `call` (its `ToolCall`'s sequence number), and, for an "always"
    /// answer, what it added to the human's rules, which the approval's
    /// reason says.
    Decision {
        call: u64,
        allow: bool,
        always: Option<String>,
    },
    /// The answer to a `Fetch` of `remote`: the store fetched and each
    /// base resolved, or why not.
    Fetched {
        remote: String,
        result: Result<Fetched, String>,
    },
    /// The commit each of `remote`'s `bases` was at when the window last
    /// fetched its store, `ids` in their order (DESIGN.md §7, Keeping
    /// current): asked for with `Heads`, and told after each fetch.
    Heads {
        remote: String,
        bases: Vec<String>,
        ids: Vec<String>,
    },
}

/// A fetched store, for a repository workspace's preparation (DESIGN.md
/// §7): the human's identity its commits carry, and each base asked for
/// resolved to its commit and its project instructions there (§13), in
/// the order asked.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Fetched {
    pub identity: crate::repo::Identity,
    pub ids: Vec<String>,
    pub instructions: Vec<crate::repo::Instructions>,
    /// Each base's `.td-agent/rules` (§11), in the same order.
    pub rules: Vec<crate::rules::Read>,
}

/// From a conversation to the window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Up {
    /// The conversation is open: its role and title, what opening it
    /// found (a torn line dropped, effects interrupted), and its prefix
    /// file's text, none when that is past `MAX_TEXT`. Escaped, that
    /// takes at most six times `MAX_TEXT` of the frame, which leaves the
    /// rest, a quarter, for the title and interrupted ids. Every event of
    /// its log follows, in order, then each new one as it is appended:
    /// `last` is the last one it replays, 0 for none.
    Hello {
        title: String,
        torn: Option<u64>,
        interrupted: Vec<u64>,
        paused: bool,
        prefix: Option<String>,
        last: u64,
    },
    Event(Event),
    /// The message with this delivery id is logged and synced.
    Delivered {
        delivery: String,
    },
    Title {
        title: String,
    },
    /// The classifier's circuit breaker tripped (DESIGN.md §11): the
    /// window puts the conversation's workspace in `ask` mode, which is
    /// all it may ask for, and says why.
    Brake {
        why: String,
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
    /// The window answers with `Decision`. `always` is what its "always"
    /// answers would remember, when it offers them.
    Ask {
        call: u64,
        title: String,
        details: Vec<String>,
        always: Option<crate::rules::Offer>,
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
    /// The conversation is done with an undo or a redo the window asked
    /// for, done or not: until then the window keeps its process
    /// (DESIGN.md §12).
    Restored,
    /// A background process's end is about to start a turn: until the
    /// turn's start, the window keeps the process (DESIGN.md §12).
    Waking,
    /// The conversation is done preparing `remote`'s repository, ready or
    /// not: until then the window keeps its process (DESIGN.md §7).
    Prepared {
        remote: String,
    },
    /// Where the window last found each of `remote`'s `bases`, answered
    /// with `Heads` of those it knows.
    Heads {
        remote: String,
        bases: Vec<String>,
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

/// A `Fetched`'s project instructions as they cross: a base at a commit
/// an earlier base names says so (`{"same": <index>}`), so a commit's
/// read crosses once however many worktrees start there.
fn carried(fetched: &Fetched) -> Json {
    let items = fetched
        .instructions
        .iter()
        .enumerate()
        .map(|(at, read)| {
            let id = fetched.ids.get(at);
            let earlier = fetched
                .ids
                .iter()
                .zip(&fetched.instructions)
                .take(at)
                .position(|(other, same)| Some(other) == id && same == read);
            match earlier {
                Some(index) => Json::Obj(vec![("same".into(), Json::from(index as u64))]),
                None => read.to_json(),
            }
        })
        .collect();
    Json::Arr(items)
}

/// A `Fetched`'s repository rules as they cross: a base at a commit an
/// earlier base names says so (`{"same": <index>}`), as the project
/// instructions do (`carried`).
fn carried_rules(fetched: &Fetched) -> Json {
    let items = fetched
        .rules
        .iter()
        .enumerate()
        .map(|(at, read)| {
            let id = fetched.ids.get(at);
            let earlier = fetched
                .ids
                .iter()
                .zip(&fetched.rules)
                .take(at)
                .position(|(other, same)| Some(other) == id && same == read);
            match earlier {
                Some(index) => Json::Obj(vec![("same".into(), Json::from(index as u64))]),
                None => read.to_json(),
            }
        })
        .collect();
    Json::Arr(items)
}

/// A `Fetched`'s repository rules, one a base of `ids`, each as it
/// crossed (`carried_rules`), within the bound of the store's answer
/// counted once a commit (DESIGN.md §11).
fn repository_rules(value: &Json, ids: &[String]) -> Result<Vec<crate::rules::Read>, String> {
    let items = value
        .get("rules")
        .and_then(Json::as_arr)
        .ok_or("no rules")?;
    if items.len() != ids.len() {
        return Err("rules for another number of bases".into());
    }
    let mut read: Vec<crate::rules::Read> = Vec::new();
    let mut carried = 0usize;
    for (at, item) in items.iter().enumerate() {
        match item.get("same") {
            Some(same) => {
                let index = same
                    .as_u64()
                    .and_then(|index| usize::try_from(index).ok())
                    .filter(|index| *index < at && ids.get(*index) == ids.get(at))
                    .ok_or("rules name no earlier base at the same commit")?;
                let earlier = read.get(index).cloned().ok_or("no such rules")?;
                read.push(earlier);
            }
            None => {
                let one = crate::rules::Read::from_json(item)?;
                carried = carried.saturating_add(one.carried());
                read.push(one);
            }
        }
    }
    if carried > crate::rules::MAX_CARRIED {
        return Err("rules past their bound".into());
    }
    Ok(read)
}

/// A `Fetched`'s project instructions, one a base of `ids`, each as it
/// crossed (`carried`), within the bound of the store's answer counted
/// once a commit.
fn instructions(value: &Json, ids: &[String]) -> Result<Vec<crate::repo::Instructions>, String> {
    let items = value
        .get("instructions")
        .and_then(Json::as_arr)
        .ok_or("no instructions")?;
    if items.len() != ids.len() {
        return Err("instructions for another number of bases".into());
    }
    let mut read: Vec<crate::repo::Instructions> = Vec::new();
    let mut carried = 0usize;
    for (at, item) in items.iter().enumerate() {
        match item.get("same") {
            Some(same) => {
                let index = same
                    .as_u64()
                    .and_then(|index| usize::try_from(index).ok())
                    .filter(|index| *index < at && ids.get(*index) == ids.get(at))
                    .ok_or("instructions name no earlier base at the same commit")?;
                let earlier = read.get(index).cloned().ok_or("no such instructions")?;
                read.push(earlier);
            }
            None => {
                let one = crate::repo::Instructions::from_json(item)?;
                carried = carried.saturating_add(one.carried());
                read.push(one);
            }
        }
    }
    if carried > crate::git::MAX_INSTRUCTIONS {
        return Err("instructions past their bound".into());
    }
    Ok(read)
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
            Self::Compact { focus } => typed(
                "compact",
                focus
                    .iter()
                    .map(|f| ("focus".into(), Json::Str(f.clone())))
                    .collect(),
            ),
            Self::Kill { number } => typed("kill", vec![("number".into(), Json::from(*number))]),
            Self::Policy {
                version,
                rules,
                mode,
            } => typed(
                "policy",
                vec![
                    ("version".into(), Json::from(*version)),
                    ("mode".into(), Json::Str(mode.word().into())),
                    match rules {
                        Ok(text) => ("rules".into(), Json::Str(text.clone())),
                        Err(why) => ("error".into(), Json::Str(why.clone())),
                    },
                ],
            ),
            Self::Restore { step, undo } => typed(
                "restore",
                vec![
                    ("step".into(), Json::from(*step)),
                    ("undo".into(), Json::Bool(*undo)),
                ],
            ),
            Self::Choose { model, effort } => typed(
                "choose",
                vec![
                    ("model".into(), optional(model)),
                    ("effort".into(), optional(effort)),
                ],
            ),
            Self::Decision {
                call,
                allow,
                always,
            } => typed(
                "decision",
                vec![
                    ("call".into(), Json::from(*call)),
                    ("allow".into(), Json::Bool(*allow)),
                    ("always".into(), optional(always)),
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
                        ("instructions".into(), carried(fetched)),
                        ("rules".into(), carried_rules(fetched)),
                    ]),
                    Err(why) => pairs.push(("error".into(), Json::Str(why.clone()))),
                }
                typed("fetched", pairs)
            }
            Self::Heads { remote, bases, ids } => typed(
                "heads",
                vec![
                    ("remote".into(), Json::Str(remote.clone())),
                    (
                        "bases".into(),
                        Json::Arr(bases.iter().cloned().map(Json::Str).collect()),
                    ),
                    (
                        "ids".into(),
                        Json::Arr(ids.iter().cloned().map(Json::Str).collect()),
                    ),
                ],
            ),
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
                Ok(Self::Setup {
                    key,
                    client: Box::new(client),
                })
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
            Some("compact") => Ok(Self::Compact {
                focus: value
                    .get("focus")
                    .and_then(Json::as_str)
                    .map(str::to_string),
            }),
            Some("policy") => Ok(Self::Policy {
                version: value
                    .get("version")
                    .and_then(Json::as_u64)
                    .ok_or("a policy with no version")?,
                mode: match value.get("mode").and_then(Json::as_str) {
                    Some("ask") => crate::config::Mode::Ask,
                    Some("auto") => crate::config::Mode::Auto,
                    _ => return Err("a policy with no mode".into()),
                },
                rules: match (value.get("rules"), value.get("error")) {
                    (Some(text), None) => {
                        let text = text.as_str().ok_or("a policy whose rules are not text")?;
                        if text.len() > crate::rules::MAX_HUMAN_FILE {
                            return Err("a policy past its bound".into());
                        }
                        Ok(text.to_string())
                    }
                    (None, Some(why)) => Err(why
                        .as_str()
                        .ok_or("a policy whose error is not text")?
                        .to_string()),
                    _ => return Err("a policy with neither rules nor an error".into()),
                },
            }),
            Some("kill") => Ok(Self::Kill {
                number: value
                    .get("number")
                    .and_then(Json::as_u64)
                    .ok_or("a kill with no number")?,
            }),
            Some("restore") => Ok(Self::Restore {
                step: value
                    .get("step")
                    .and_then(Json::as_u64)
                    .ok_or("a restore with no step")?,
                undo: value
                    .get("undo")
                    .and_then(Json::as_bool)
                    .ok_or("a restore with no undo")?,
            }),
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
                always: match maybe(&value, "always")? {
                    Some(text) if text.len() > MAX_REMEMBERED => {
                        return Err("a decision whose remembered rules are past their bound".into())
                    }
                    always => always,
                },
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
                        instructions: instructions(&value, &strings(&value, "ids")?)?,
                        rules: repository_rules(&value, &strings(&value, "ids")?)?,
                    }),
                },
            }),
            Some("heads") => {
                let bases = strings(&value, "bases")?;
                let ids = strings(&value, "ids")?;
                if bases.len() != ids.len() {
                    return Err("heads holds a base without its commit".into());
                }
                Ok(Self::Heads {
                    remote: string(&value, "remote")?,
                    bases,
                    ids,
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
                title,
                torn,
                interrupted,
                paused,
                prefix,
                last,
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
                    ("last".into(), Json::from(*last)),
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
            Self::Brake { why } => typed("brake", vec![("why".into(), Json::Str(why.clone()))]),
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
            Self::Restored => typed("restored", Vec::new()),
            Self::Waking => typed("waking", Vec::new()),
            Self::Prepared { remote } => typed(
                "prepared",
                vec![("remote".into(), Json::Str(remote.clone()))],
            ),
            Self::Heads { remote, bases } => typed(
                "heads",
                vec![
                    ("remote".into(), Json::Str(remote.clone())),
                    (
                        "bases".into(),
                        Json::Arr(bases.iter().cloned().map(Json::Str).collect()),
                    ),
                ],
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
                always,
            } => {
                let mut members = vec![
                    ("call".into(), Json::from(*call)),
                    ("title".into(), Json::Str(title.clone())),
                    (
                        "details".into(),
                        Json::Arr(details.iter().cloned().map(Json::Str).collect()),
                    ),
                ];
                match always {
                    None => {}
                    Some(crate::rules::Offer::Rules(always)) => members.push((
                        "always".into(),
                        Json::Obj(vec![
                            ("allow".into(), Json::Bool(always.allow)),
                            (
                                "rules".into(),
                                Json::Arr(always.bodies.iter().cloned().map(Json::Str).collect()),
                            ),
                        ]),
                    )),
                    Some(crate::rules::Offer::Crossing { op, to }) => members.push((
                        "always".into(),
                        Json::Obj(vec![
                            ("crossing".into(), Json::Str(op.name().into())),
                            ("to".into(), Json::Str(to.clone())),
                        ]),
                    )),
                }
                typed("ask", members)
            }
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
                last: value.get("last").and_then(Json::as_u64).ok_or("no last")?,
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
            Some("brake") => Self::Brake {
                why: string(&value, "why")?,
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
                always: match value.get("always") {
                    None => None,
                    Some(always) => Some(match always.get("crossing") {
                        Some(op) => crate::rules::Offer::Crossing {
                            op: op
                                .as_str()
                                .and_then(crate::rules::Crossed::parse)
                                .ok_or("a crossing that neither reads nor messages")?,
                            to: crate::store::Id::parse(&string(always, "to")?)
                                .ok_or("a crossing to no conversation")?
                                .as_str()
                                .to_string(),
                        },
                        None => crate::rules::Offer::Rules(crate::rules::Always::checked(
                            always
                                .get("allow")
                                .and_then(Json::as_bool)
                                .ok_or("always without allow")?,
                            strings(always, "rules")?,
                        )?),
                    }),
                },
            },
            Some("withdraw") => Self::Withdraw {
                call: number(&value, "call")?,
            },
            Some("fetch") => Self::Fetch {
                remote: string(&value, "remote")?,
                bases: strings(&value, "bases")?,
            },
            Some("restored") => Self::Restored,
            Some("waking") => Self::Waking,
            Some("prepared") => Self::Prepared {
                remote: string(&value, "remote")?,
            },
            Some("heads") => Self::Heads {
                remote: string(&value, "remote")?,
                bases: strings(&value, "bases")?,
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
    fn a_fetched_answer_carries_instructions_within_their_bound() {
        // Each text at its own commit, or all at one.
        let at = |n: usize, one: bool| -> Vec<String> {
            (0..n)
                .map(|i| format!("{:040x}", if one { 0 } else { i }))
                .collect()
        };
        let fetched = |texts: Vec<String>, one: bool| Down::Fetched {
            remote: "https://github.com/timmydo/td".into(),
            result: Ok(Fetched {
                identity: crate::repo::Identity::default(),
                ids: at(texts.len(), one),
                rules: vec![crate::rules::Read::Absent; texts.len()],
                instructions: texts
                    .into_iter()
                    .map(|text| crate::repo::Instructions::Found {
                        name: "AGENTS.md".into(),
                        text,
                    })
                    .collect(),
            }),
        };
        // At its bound, the worst escaping still fits a frame.
        let most = fetched(vec!["\u{1}".repeat(crate::git::MAX_INSTRUCTIONS)], false);
        let bytes = most.encode();
        assert!(bytes.len() <= crate::frame::MAX_FRAME, "{}", bytes.len());
        assert_eq!(Down::decode(&bytes).unwrap(), most);
        let half = "x".repeat(crate::git::MAX_INSTRUCTIONS / 2 + 1);
        let past = fetched(vec![half.clone(), half.clone()], false);
        assert!(Down::decode(&past.encode()).is_err());
        // A commit's text crosses once, however many worktrees start
        // there: as many as a workspace has, at the bound, fit a frame.
        let shared = fetched(
            vec!["\u{1}".repeat(crate::git::MAX_INSTRUCTIONS); MAX_STRINGS],
            true,
        );
        let bytes = shared.encode();
        assert!(bytes.len() <= crate::frame::MAX_FRAME, "{}", bytes.len());
        assert_eq!(Down::decode(&bytes).unwrap(), shared);
        // A back-reference names an earlier base at the same commit.
        let forged = br#"{"type":"fetched","remote":"https://github.com/timmydo/td","error":null,"name":null,"email":null,"ids":["0000000000000000000000000000000000000000","0000000000000000000000000000000000000001"],"instructions":[{"kind":"absent"},{"same":0}],"rules":[{"kind":"absent"},{"kind":"absent"}]}"#;
        assert!(Down::decode(forged).is_err());
    }

    #[test]
    fn a_fetched_answer_carries_rules_within_their_bound_beside_the_most_instructions() {
        use crate::rules::{Read, Rule, MAX_CARRIED, MAX_WHY};
        // Every base at its own commit, or all at one; the most
        // instructions, at their worst escaping, at the first commit.
        let answer = |rules: Vec<Read>, one: bool| Down::Fetched {
            remote: "https://github.com/timmydo/td".into(),
            result: Ok(Fetched {
                identity: crate::repo::Identity::default(),
                ids: (0..MAX_STRINGS)
                    .map(|i| format!("{:040x}", if one { 0 } else { i }))
                    .collect(),
                instructions: (0..MAX_STRINGS)
                    .map(|i| {
                        if i == 0 || one {
                            crate::repo::Instructions::Found {
                                name: "AGENTS.md".into(),
                                text: "\u{1}".repeat(crate::git::MAX_INSTRUCTIONS),
                            }
                        } else {
                            crate::repo::Instructions::Absent
                        }
                    })
                    .collect(),
                rules,
            }),
        };
        // Every commit's rules, at the bound, beside the worst
        // instructions.
        let rule = Rule::parse(&format!("deny shell {}", "x".repeat(500))).unwrap();
        assert_eq!(rule.text().len() + 1, MAX_CARRIED / MAX_STRINGS / 2);
        let found = Read::Found(vec![rule.clone(), rule.clone()]);
        let most = answer(vec![found.clone(); MAX_STRINGS], false);
        let bytes = most.encode();
        assert!(bytes.len() <= crate::frame::MAX_FRAME, "{}", bytes.len());
        assert_eq!(Down::decode(&bytes).unwrap(), most);
        // Every base's reason, at its bound and escaped at its worst.
        let unread = Read::unread(&"\u{1}".repeat(MAX_WHY));
        let most = answer(vec![unread; MAX_STRINGS], false);
        let bytes = most.encode();
        assert!(bytes.len() <= crate::frame::MAX_FRAME, "{}", bytes.len());
        assert_eq!(Down::decode(&bytes).unwrap(), most);
        // A commit's rules cross once, however many worktrees start
        // there: every base at one commit, each at the whole bound.
        let whole = Read::Found(vec![rule.clone(); MAX_CARRIED / 512]);
        assert_eq!(whole.carried(), MAX_CARRIED);
        let shared = answer(vec![whole; MAX_STRINGS], true);
        let bytes = shared.encode();
        assert!(bytes.len() <= crate::frame::MAX_FRAME, "{}", bytes.len());
        assert_eq!(Down::decode(&bytes).unwrap(), shared);
        // Past the bound, the answer is refused.
        let mut past = vec![found; MAX_STRINGS];
        *past.first_mut().unwrap() = Read::Found(vec![rule.clone(), rule.clone(), rule]);
        assert!(Down::decode(&answer(past, false).encode()).is_err());
        // A back-reference names an earlier base at the same commit.
        let forged = format!(
            r#"{{"type":"fetched","remote":"https://github.com/timmydo/td","error":null,"name":null,"email":null,"ids":["{0}","{1}"],"instructions":[{{"kind":"absent"}},{{"kind":"absent"}}],"rules":[{{"kind":"absent"}},{{"same":0}}]}}"#,
            "0".repeat(40),
            "1".repeat(40)
        );
        assert!(Down::decode(forged.as_bytes()).is_err());
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
                last: 7,
            },
            Up::Hello {
                title: "t".into(),
                torn: None,
                interrupted: vec![],
                paused: true,
                prefix: None,
                last: 0,
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
            Up::Restored,
            Up::Waking,
            Up::Heads {
                remote: "https://github.com/timmydo/td".into(),
                bases: vec!["main".into()],
            },
            Up::Ask {
                call: 9,
                title: "Run a command?".into(),
                details: vec!["In /w:".into(), "cargo test".into()],
                always: None,
            },
            Up::Ask {
                call: 10,
                title: "Run a command?".into(),
                details: vec!["cargo test && rm x".into()],
                always: Some(crate::rules::Offer::Rules(crate::rules::Always {
                    allow: true,
                    bodies: vec!["shell cargo test".into(), "shell rm".into()],
                })),
            },
            Up::Ask {
                call: 11,
                title: "Read another conversation's log".into(),
                details: vec!["Conversation ...".into()],
                always: Some(crate::rules::Offer::Crossing {
                    op: crate::rules::Crossed::Read,
                    to: "b".repeat(32),
                }),
            },
            Up::Ask {
                call: 12,
                title: "Send a message to another conversation".into(),
                details: vec!["hello".into()],
                always: Some(crate::rules::Offer::Crossing {
                    op: crate::rules::Crossed::Message,
                    to: "c".repeat(32),
                }),
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
            Up::Brake {
                why: "it did not allow 3 actions in a row".into(),
            },
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
            Down::Compact { focus: None },
            Down::Compact {
                focus: Some("the failing test".into()),
            },
            Down::Kill { number: 3 },
            Down::Policy {
                version: 2,
                rules: Ok("[everywhere]\ndeny shell rm\n".into()),
                mode: crate::config::Mode::Auto,
            },
            Down::Policy {
                version: 3,
                rules: Err("rules: line 1: names no tool".into()),
                mode: crate::config::Mode::Ask,
            },
            Down::Restore {
                step: 7,
                undo: true,
            },
            Down::Restore {
                step: 9,
                undo: false,
            },
            Down::Decision {
                call: 7,
                allow: true,
                always: None,
            },
            Down::Decision {
                call: 8,
                allow: false,
                always: Some("`deny shell rm` in your rules for every workspace".into()),
            },
            Down::Choose {
                model: Some("openai/gpt-6".into()),
                effort: Some("high".into()),
            },
            Down::Choose {
                model: None,
                effort: None,
            },
            Down::Heads {
                remote: "https://github.com/timmydo/td".into(),
                bases: vec!["main".into(), "next".into()],
                ids: vec!["a".repeat(40), "b".repeat(40)],
            },
        ] {
            assert!(down.encode().len() <= crate::frame::MAX_FRAME);
            assert_eq!(Down::decode(&down.encode()).unwrap(), down);
        }
        // A base without its commit is no answer.
        let lopsided = br#"{"type":"heads","remote":"https://github.com/timmydo/td","bases":["main","next"],"ids":["aaaa"]}"#;
        assert!(Down::decode(lopsided).is_err());
        let client = Client::default();
        for down in [
            Down::Setup {
                key: Ok(Secret::new("sk-or-v1-abc".into())),
                client: Box::new(client.clone()),
            },
            Down::Setup {
                key: Err("no API key".into()),
                client: Box::new(client),
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
                    ids: vec!["a".repeat(40), "b".repeat(40), "c".repeat(40)],
                    instructions: vec![
                        crate::repo::Instructions::Found {
                            name: "AGENTS.md".into(),
                            text: "Run `make`.\n\u{1}".into(),
                        },
                        crate::repo::Instructions::Absent,
                        crate::repo::Instructions::Unread {
                            why: "AGENTS.md is not UTF-8".into(),
                        },
                    ],
                    rules: vec![
                        crate::rules::Read::Found(vec![crate::rules::Rule::parse(
                            "ask shell git push",
                        )
                        .unwrap()]),
                        crate::rules::Read::Absent,
                        crate::rules::Read::unread(".td-agent/rules is not UTF-8"),
                    ],
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
            client: Box::default(),
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
