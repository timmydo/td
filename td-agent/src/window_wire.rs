//! The window's and its session's messages, `View` and `Request`, as bytes
//! (DESIGN.md §2): one JSON object a message, a `"type"` naming its variant,
//! as `protocol`'s are. What decodes here is untrusted, and every fault in it
//! is an `Err`; a fault in a `SaveKey` does not quote what was sent, so a key
//! reaches no error. What goes on to a file or a path is held to that file's or
//! path's own rule here, as the sink holds it again.
//!
//! Each payload with a form of its own keeps it, unchanged. Those forms write
//! a path as text, which a path that is not UTF-8 does not survive and which
//! they must go on writing, so `carried` puts such a form's paths beside it as
//! bytes when any needs that. Bytes go as base64 (`bytes_json`), the shortest
//! of td-encoding's forms, and its decoder takes one spelling of a value.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::sync::Arc;

use td_json::Json;

use crate::book::{closing_from, Book, Row};
use crate::card::Record;
use crate::chooser::{Folders, FoldersAnswer};
use crate::config::{Client, Mode, Template};
use crate::cost::Limits;
use crate::key::Secret;
use crate::output::Output;
use crate::picker::Offer;
use crate::picture::Reference;
use crate::protocol::{
    maybe, number, offer_from_json, offer_json, optional, string, typed, Resumed, Up,
};
use crate::rules::{Always, Crossed};
use crate::schedule::Command;
use crate::skills::{Found, Skill};
use crate::store::{instructions_from_json, instructions_json, Event, Id, Instructed, TaskStart};
use crate::supervisor::Update;
use crate::ui::{Answer, Asked, Attached, Card, Request};
use crate::view::{Pictures, View};
use crate::workspace::Workspace;

type Pairs = Vec<(String, Json)>;

fn field(name: &str, value: Json) -> (String, Json) {
    (name.to_string(), value)
}

fn text(value: &str) -> Json {
    Json::Str(value.to_string())
}

fn texts(values: &[String]) -> Json {
    Json::Arr(values.iter().map(|value| text(value)).collect())
}

fn id_json(id: &Id) -> Json {
    Json::Str(id.to_string())
}

/// Two texts a row, as schedules, commands and debug views are.
fn rows_json(rows: &[(String, String)]) -> Json {
    Json::Arr(
        rows.iter()
            .map(|(first, second)| Json::Arr(vec![text(first), text(second)]))
            .collect(),
    )
}

/// Bytes as text: the one form of bytes: pictures, and any path that is carried exactly.
fn bytes_json(bytes: &[u8]) -> Json {
    Json::Str(td_encoding::base64(bytes))
}

fn bytes_from(value: &Json) -> Result<Vec<u8>, String> {
    value
        .as_str()
        .and_then(td_encoding::from_base64)
        .ok_or_else(|| "bytes that are not base64".to_string())
}

fn path_json(path: &std::path::Path) -> Json {
    bytes_json(path.as_os_str().as_bytes())
}

fn path_from(value: &Json) -> Result<PathBuf, String> {
    Ok(PathBuf::from(OsString::from_vec(bytes_from(value)?)))
}

fn paths_json(paths: &[PathBuf]) -> Json {
    Json::Arr(paths.iter().map(|path| path_json(path)).collect())
}

fn folders_json(folders: &Folders) -> Json {
    Json::Obj(vec![
        field("folder", path_json(&folders.folder)),
        field("entries", rows_json(&folders.entries)),
        field("truncated", Json::Bool(folders.truncated)),
    ])
}

/// As it was said: `chooser::Folders::listing` holds it to what can be shown.
fn folders_from(value: &Json) -> Result<Folders, String> {
    Ok(Folders {
        folder: member(value, "folder", path_from)?,
        entries: member(value, "entries", rows_from)?,
        truncated: flag(value, "truncated")?,
    })
}

/// A result, whichever it is, as one member of two.
fn result_json<T>(result: &Result<T, String>, ok: impl FnOnce(&T) -> Json) -> Json {
    Json::Obj(vec![match result {
        Ok(value) => field("ok", ok(value)),
        Err(why) => field("err", text(why)),
    }])
}

/// `result_json`'s value back: it holds the one, or the other, never both.
fn result_from<T>(
    value: &Json,
    ok: impl FnOnce(&Json) -> Result<T, String>,
) -> Result<Result<T, String>, String> {
    match (value.get("ok"), value.get("err")) {
        (Some(read), None) => Ok(Ok(ok(read)?)),
        (None, Some(why)) => Ok(Err(why
            .as_str()
            .ok_or("a reason that is not text")?
            .to_string())),
        _ => Err("a result that is neither or both".into()),
    }
}

/// Member `name`, read by `read`.
fn member<T>(
    value: &Json,
    name: &str,
    read: impl FnOnce(&Json) -> Result<T, String>,
) -> Result<T, String> {
    read(value.get(name).ok_or_else(|| format!("no {name}"))?)
}

pub(crate) fn flag(value: &Json, name: &str) -> Result<bool, String> {
    value
        .get(name)
        .and_then(Json::as_bool)
        .ok_or_else(|| format!("no {name}"))
}

pub(crate) fn id_from(value: &Json, name: &str) -> Result<Id, String> {
    Id::parse(&string(value, name)?).ok_or_else(|| format!("{name} is not a conversation's id"))
}

pub(crate) fn list<'a>(value: &'a Json, name: &str) -> Result<&'a [Json], String> {
    value
        .get(name)
        .and_then(Json::as_arr)
        .ok_or_else(|| format!("no {name}"))
}

pub(crate) fn numbers(value: &Json, name: &str) -> Result<Vec<u64>, String> {
    list(value, name)?
        .iter()
        .map(|item| {
            item.as_u64()
                .ok_or_else(|| format!("{name} holds something other than a number"))
        })
        .collect()
}

fn items<T>(
    value: &Json,
    name: &str,
    read: impl Fn(&Json) -> Result<T, String>,
) -> Result<Vec<T>, String> {
    list(value, name)?.iter().map(read).collect()
}

fn strings(value: &Json, name: &str) -> Result<Vec<String>, String> {
    items(value, name, |item| {
        item.as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("{name} holds something other than text"))
    })
}

fn rows_from(value: &Json) -> Result<Vec<(String, String)>, String> {
    value
        .as_arr()
        .ok_or("rows that are not a list")?
        .iter()
        .map(|row| match row.as_arr() {
            Some([Json::Str(first), Json::Str(second)]) => Ok((first.clone(), second.clone())),
            _ => Err("a row that is not two texts".to_string()),
        })
        .collect()
}

/// The save an answer about a prompt is to.
fn ask_number(value: &Json) -> Result<u64, String> {
    maybe_number(value, "ask")?.ok_or_else(|| "ask is not a number".to_string())
}

fn maybe_number(value: &Json, name: &str) -> Result<Option<u64>, String> {
    match value.get(name) {
        Some(Json::Null) => Ok(None),
        Some(item) => item
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("{name} is not a number")),
        None => Err(format!("no {name}")),
    }
}

fn narrow(value: &Json, name: &str) -> Result<u32, String> {
    u32::try_from(number(value, name)?).map_err(|_| format!("{name} is out of range"))
}

fn size(value: &Json, name: &str) -> Result<usize, String> {
    usize::try_from(number(value, name)?).map_err(|_| format!("{name} is out of range"))
}

fn mode_from(value: &Json, name: &str) -> Result<Mode, String> {
    Mode::parse(&string(value, name)?).ok_or_else(|| format!("{name} is no mode"))
}

/// What reaches every path of a value, in one order.
type EachPath<T> = fn(&mut T, &mut dyn FnMut(&mut PathBuf));

/// A form that writes paths as text, and them as bytes beside it, when any
/// needs that.
fn carried<T: Clone>(value: &T, form: Json, visit: EachPath<T>) -> Json {
    let mut copy = value.clone();
    let mut paths = Vec::new();
    visit(&mut copy, &mut |path| paths.push(std::mem::take(path)));
    let mut pairs = vec![field("form", form)];
    if paths.iter().any(|path| path.to_str().is_none()) {
        pairs.push(field(
            "paths",
            Json::Arr(paths.iter().map(|path| path_json(path)).collect()),
        ));
    }
    Json::Obj(pairs)
}

/// `carried`'s value back: the form read by `read`, which checks what it
/// says of its paths, and each exact path then held to read as that one.
fn carried_from<T>(
    value: &Json,
    read: impl FnOnce(&Json) -> Result<T, String>,
    visit: EachPath<T>,
) -> Result<T, String> {
    let mut read = member(value, "form", read)?;
    if value.get("paths").is_some() {
        let exact = items(value, "paths", path_from)?;
        let mut exact = exact.into_iter();
        let mut fits = true;
        visit(&mut read, &mut |path| match exact.next() {
            Some(bytes) if bytes.to_string_lossy() == path.to_string_lossy() => *path = bytes,
            _ => fits = false,
        });
        if !fits || exact.next().is_some() {
            return Err("exact paths that are not the form's".into());
        }
    }
    Ok(read)
}

fn workspace_paths(workspace: &mut Workspace, visit: &mut dyn FnMut(&mut PathBuf)) {
    match workspace {
        Workspace::Scratch | Workspace::Template(_) => {}
        Workspace::Directory(path) => visit(path),
        Workspace::Repositories(repositories) => {
            for entry in &mut repositories.entries {
                visit(&mut entry.store);
                visit(&mut entry.repository);
                visit(&mut entry.checkout);
            }
        }
    }
}

pub(crate) fn workspace_json(workspace: &Workspace) -> Json {
    carried(workspace, workspace.to_json(), workspace_paths)
}

pub(crate) fn workspace_from_json(value: &Json) -> Result<Workspace, String> {
    carried_from(value, Workspace::from_json, workspace_paths)
}

fn client_paths(client: &mut Client, visit: &mut dyn FnMut(&mut PathBuf)) {
    let own = client
        .template_shared
        .iter_mut()
        .flat_map(|template| template.shared.iter_mut().flatten());
    for shared in client.shared.iter_mut().chain(own) {
        visit(&mut shared.path);
    }
}

fn template_paths(template: &mut Template, visit: &mut dyn FnMut(&mut PathBuf)) {
    for shared in template.shared.iter_mut().flatten() {
        visit(&mut shared.path);
    }
}

/// A template: its file's form, which has no room for MCP servers since a
/// template made in the window names none, and those.
fn template_json(template: &Template) -> Json {
    Json::Obj(vec![
        field(
            "template",
            carried(
                template,
                crate::config::template_json(template),
                template_paths,
            ),
        ),
        field(
            "mcp",
            Json::Arr(
                template
                    .mcp
                    .iter()
                    .map(crate::config::McpServer::to_json)
                    .collect(),
            ),
        ),
    ])
}

fn template_from(value: &Json) -> Result<Template, String> {
    let mut template = member(value, "template", |form| {
        carried_from(form, crate::config::template_from_json, template_paths)
    })?;
    template.mcp = member(value, "mcp", crate::config::mcp_from_json)?;
    Ok(template)
}

fn instructed_paths(all: &mut Vec<Instructed>, visit: &mut dyn FnMut(&mut PathBuf)) {
    for one in all {
        visit(&mut one.checkout);
    }
}

fn record_json(record: &Record) -> Json {
    Json::Obj(vec![
        field(
            "instructions",
            result_json(&record.instructions, |all| {
                carried(all, instructions_json(all), instructed_paths)
            }),
        ),
        field(
            "prepared",
            result_json(&record.prepared, |paths| {
                Json::Arr(paths.iter().map(|path| path_json(path)).collect())
            }),
        ),
        field("removed", Json::Bool(record.removed)),
        field("trusted", result_json(&record.trusted, optional)),
    ])
}

fn record_from(value: &Json) -> Result<Record, String> {
    Ok(Record {
        instructions: member(value, "instructions", |result| {
            result_from(result, |all| {
                carried_from(all, instructions_from_json, instructed_paths)
            })
        })?,
        prepared: member(value, "prepared", |result| {
            result_from(result, |paths| {
                paths
                    .as_arr()
                    .ok_or("prepared paths that are not a list")?
                    .iter()
                    .map(path_from)
                    .collect()
            })
        })?,
        removed: flag(value, "removed")?,
        trusted: member(value, "trusted", |result| {
            result_from(result, |digest| match digest {
                Json::Null => Ok(None),
                Json::Str(digest) => Ok(Some(digest.clone())),
                _ => Err("a digest that is not text".to_string()),
            })
        })?,
    })
}

fn references_json(references: &[Reference]) -> Json {
    Json::Arr(references.iter().map(Reference::to_json).collect())
}

fn attached_json(attached: &[Attached]) -> Json {
    Json::Arr(
        attached
            .iter()
            .map(|(reference, bytes)| {
                Json::Obj(vec![
                    field("reference", reference.to_json()),
                    field("bytes", bytes_json(bytes)),
                ])
            })
            .collect(),
    )
}

fn attached_from(value: &Json, name: &str) -> Result<Vec<Attached>, String> {
    items(value, name, |one| {
        Ok((
            member(one, "reference", Reference::from_json)?,
            Arc::from(member(one, "bytes", bytes_from)?),
        ))
    })
}

fn pictures_json(pictures: &Pictures) -> Json {
    Json::Arr(
        pictures
            .iter()
            .map(|(digest, read)| {
                Json::Obj(vec![
                    field("digest", text(digest)),
                    field("read", result_json(read, |bytes| bytes_json(bytes))),
                ])
            })
            .collect(),
    )
}

fn pictures_from(value: &Json) -> Result<Pictures, String> {
    let mut pictures = Pictures::new();
    for one in list(value, "pictures")? {
        let read = member(one, "read", |read| result_from(read, bytes_from))?;
        if pictures.insert(string(one, "digest")?, read).is_some() {
            return Err("a picture named twice".into());
        }
    }
    Ok(pictures)
}

fn update_json(update: &Update) -> Json {
    let (kind, pairs) = match update {
        Update::Up(up) => ("up", vec![field("up", up.to_json())]),
        Update::Refused {
            text: refused,
            pictures,
            reason,
        } => (
            "refused",
            vec![
                field("text", optional(refused)),
                field("pictures", references_json(pictures)),
                field("reason", text(reason)),
            ],
        ),
        Update::Restarting { reason } => ("restarting", vec![field("reason", text(reason))]),
        Update::Failed { reason } => ("failed", vec![field("reason", text(reason))]),
        Update::Undeliverable { delivery, reason } => (
            "undeliverable",
            vec![
                field("delivery", text(delivery)),
                field("reason", text(reason)),
            ],
        ),
    };
    let mut all = vec![field("type", text(kind))];
    all.extend(pairs);
    Json::Obj(all)
}

fn update_from(value: &Json) -> Result<Update, String> {
    match value.get("type").and_then(Json::as_str) {
        Some("up") => Ok(Update::Up(member(value, "up", Up::from_json)?)),
        Some("refused") => Ok(Update::Refused {
            text: maybe(value, "text")?,
            pictures: items(value, "pictures", Reference::from_json)?,
            reason: string(value, "reason")?,
        }),
        Some("restarting") => Ok(Update::Restarting {
            reason: string(value, "reason")?,
        }),
        Some("failed") => Ok(Update::Failed {
            reason: string(value, "reason")?,
        }),
        Some("undeliverable") => Ok(Update::Undeliverable {
            delivery: string(value, "delivery")?,
            reason: string(value, "reason")?,
        }),
        _ => Err("an update of an unknown type".into()),
    }
}

fn card_json(card: &Card) -> Json {
    Json::Obj(vec![
        field("conversation", id_json(&card.conversation)),
        field("call", Json::from(card.call)),
        field("title", text(&card.title)),
        field("details", texts(&card.details)),
        field(
            "always",
            card.always.as_ref().map_or(Json::Null, offer_json),
        ),
        field("resume", Json::Bool(card.resume)),
    ])
}

fn card_from(value: &Json) -> Result<Card, String> {
    Ok(Card {
        conversation: id_from(value, "conversation")?,
        call: number(value, "call")?,
        title: string(value, "title")?,
        details: strings(value, "details")?,
        always: match value.get("always") {
            Some(Json::Null) => None,
            Some(always) => Some(offer_from_json(always)?),
            None => return Err("no always".into()),
        },
        resume: flag(value, "resume")?,
    })
}

fn asked_json(asked: &Asked) -> Json {
    Json::Obj(vec![
        field("conversation", id_json(&asked.conversation)),
        field("call", Json::from(asked.call)),
        field("question", text(&asked.question)),
        field("options", texts(&asked.options)),
    ])
}

fn asked_from(value: &Json) -> Result<Asked, String> {
    Ok(Asked {
        conversation: id_from(value, "conversation")?,
        call: number(value, "call")?,
        question: string(value, "question")?,
        options: strings(value, "options")?,
    })
}

fn answer_json(answer: &Answer) -> Json {
    Json::Obj(match answer {
        Answer::AllowAll => vec![
            field("answer", text("allow_all")),
            field("allow", Json::Bool(true)),
        ],
        Answer::Once(allow) => vec![
            field("answer", text("once")),
            field("allow", Json::Bool(*allow)),
        ],
        Answer::Always {
            allow,
            everywhere,
            bodies,
        } => vec![
            field("answer", text("always")),
            field("allow", Json::Bool(*allow)),
            field("everywhere", Json::Bool(*everywhere)),
            field("bodies", texts(bodies)),
        ],
        Answer::Crossing { allow, op, to } => vec![
            field("answer", text("crossing")),
            field("allow", Json::Bool(*allow)),
            field("op", text(op.name())),
            field("to", text(to)),
        ],
    })
}

/// An answer, its rules and its crossing checked as the card that offers
/// them has them, since an always answer goes into the human's rules.
fn answer_from(value: &Json) -> Result<Answer, String> {
    let allow = flag(value, "allow")?;
    match value.get("answer").and_then(Json::as_str) {
        Some("once") => Ok(Answer::Once(allow)),
        Some("allow_all") if allow => Ok(Answer::AllowAll),
        Some("always") => Ok(Answer::Always {
            allow,
            everywhere: flag(value, "everywhere")?,
            bodies: Always::checked(allow, strings(value, "bodies")?)?.bodies,
        }),
        Some("crossing") => Ok(Answer::Crossing {
            allow,
            op: Crossed::parse(&string(value, "op")?).ok_or("a crossing of no known kind")?,
            to: id_from(value, "to")?.to_string(),
        }),
        _ => Err("an answer of an unknown kind".into()),
    }
}

fn command_json(command: &Command) -> Json {
    Json::Obj(match command {
        Command::Add {
            when,
            text: said,
            catch_up,
        } => vec![
            field("command", text("add")),
            field("when", text(when)),
            field("text", text(said)),
            field("catch_up", Json::Bool(*catch_up)),
        ],
        Command::List => vec![field("command", text("list"))],
        Command::Remove(schedule) => vec![
            field("command", text("remove")),
            field("schedule", text(schedule)),
        ],
    })
}

fn command_from(value: &Json) -> Result<Command, String> {
    match value.get("command").and_then(Json::as_str) {
        Some("add") => Ok(Command::Add {
            when: string(value, "when")?,
            text: string(value, "text")?,
            catch_up: flag(value, "catch_up")?,
        }),
        Some("list") => Ok(Command::List),
        Some("remove") => Ok(Command::Remove(string(value, "schedule")?)),
        _ => Err("a schedule command of an unknown kind".into()),
    }
}

fn offer_row_json(offer: &Offer) -> Json {
    Json::Obj(vec![
        field("id", text(&offer.id)),
        field("price", text(&offer.price)),
        field("usable", Json::Bool(offer.usable)),
        field("reasoning", Json::Bool(offer.reasoning)),
    ])
}

fn offer_row_from(value: &Json) -> Result<Offer, String> {
    Ok(Offer {
        id: string(value, "id")?,
        price: string(value, "price")?,
        usable: flag(value, "usable")?,
        reasoning: flag(value, "reasoning")?,
    })
}

fn found_json(found: &Found) -> Json {
    Json::Obj(vec![
        field(
            "skills",
            Json::Arr(
                found
                    .skills
                    .iter()
                    .map(|skill| {
                        Json::Obj(vec![
                            field("name", text(&skill.name)),
                            field("description", text(&skill.description)),
                        ])
                    })
                    .collect(),
            ),
        ),
        field("refused", rows_json(&found.refused)),
        field("more", Json::from(found.more as u64)),
    ])
}

fn found_from(value: &Json) -> Result<Found, String> {
    Ok(Found {
        skills: items(value, "skills", |skill| {
            Ok(Skill {
                name: string(skill, "name")?,
                description: string(skill, "description")?,
            })
        })?,
        refused: member(value, "refused", rows_from)?,
        more: size(value, "more")?,
    })
}

fn output_json(output: &Output) -> Json {
    Json::Obj(vec![
        field("text", text(&output.text)),
        field("from", Json::from(output.from)),
        field("to", Json::from(output.to)),
        field("start", Json::from(output.start)),
        field("total", Json::from(output.total)),
    ])
}

fn output_from(value: &Json) -> Result<Output, String> {
    Ok(Output {
        text: string(value, "text")?,
        from: number(value, "from")?,
        to: number(value, "to")?,
        start: number(value, "start")?,
        total: number(value, "total")?,
    })
}

fn task_json(task: &TaskStart) -> Json {
    Json::Obj(vec![
        field("seq", Json::from(task.seq)),
        field("call", Json::from(task.call)),
        field("agent", text(&task.agent)),
        field("prompt", text(&task.prompt)),
    ])
}

fn task_from(value: &Json) -> Result<TaskStart, String> {
    Ok(TaskStart {
        seq: number(value, "seq")?,
        call: number(value, "call")?,
        agent: string(value, "agent")?,
        prompt: string(value, "prompt")?,
    })
}

fn row_list_json(rows: &[Row]) -> Json {
    Json::Arr(rows.iter().map(Row::to_json).collect())
}

/// What a frame holds, the parser's words, which may quote a key of it, left
/// behind.
fn frame(bytes: &[u8]) -> Result<Json, String> {
    td_json::parse_slice(bytes).map_err(|e| format!("not JSON, at byte {}", e.offset))
}

/// One message from the session to the window.
pub fn encode_view(view: &View) -> Vec<u8> {
    let (kind, pairs): (&str, Pairs) = match view {
        View::Opened(book) => ("opened", vec![field("book", book.to_json())]),
        View::SetMode(mode) => (
            "set_mode",
            vec![field("mode", Json::Str(mode.word().into()))],
        ),
        View::Counted(id, events, ended) => (
            "counted",
            vec![
                field("id", id_json(id)),
                field(
                    "events",
                    Json::Arr(events.iter().map(Event::to_json).collect()),
                ),
                field(
                    "ended",
                    Json::Arr(ended.iter().map(|call| Json::from(*call)).collect()),
                ),
            ],
        ),
        View::SetShare(share) => (
            "set_share",
            vec![field(
                "share",
                share.map_or(Json::Null, |(first, total)| {
                    Json::Arr(vec![Json::from(first), Json::from(total)])
                }),
            )],
        ),
        View::Note(message) => ("note", vec![field("message", text(message))]),
        View::Withdraw(conversation, call) => (
            "withdraw",
            vec![
                field("conversation", id_json(conversation)),
                field("call", call.map_or(Json::Null, Json::from)),
            ],
        ),
        View::SetActive(id) => ("set_active", vec![field("id", id_json(id))]),
        View::Update(of, update, now, pictures) => (
            "update",
            vec![
                field("of", of.as_ref().map_or(Json::Null, id_json)),
                field("update", update_json(update)),
                field("now", Json::from(*now)),
                field("pictures", pictures_json(pictures)),
            ],
        ),
        View::Background(id, update, now) => (
            "background",
            vec![
                field("id", id_json(id)),
                field("update", update_json(update)),
                field("now", Json::from(*now)),
            ],
        ),
        View::Replay(of, prefix, events, pictures) => (
            "replay",
            vec![
                field("of", of.as_ref().map_or(Json::Null, id_json)),
                field("prefix", result_json(prefix, |prefix| text(prefix))),
                field(
                    "events",
                    Json::Arr(events.iter().map(Event::to_json).collect()),
                ),
                field("pictures", pictures_json(pictures)),
            ],
        ),
        View::GiveBack(said, pictures, why) => (
            "give_back",
            vec![
                field("text", text(said)),
                field("pictures", attached_json(pictures)),
                field("why", text(why)),
            ],
        ),
        View::Restore(said, why) => (
            "restore",
            vec![field("text", text(said)), field("why", text(why))],
        ),
        View::PromptSaved(ask, said) => (
            "prompt_saved",
            vec![field("ask", Json::from(*ask)), field("said", text(said))],
        ),
        View::PromptRefused(ask, why) => (
            "prompt_refused",
            vec![field("ask", Json::from(*ask)), field("why", text(why))],
        ),
        View::TemplateSaved(said) => ("template_saved", vec![field("said", text(said))]),
        View::TemplateRefused(why) => ("template_refused", vec![field("why", text(why))]),
        View::SetTemplates(templates) => (
            "set_templates",
            vec![field(
                "templates",
                Json::Arr(
                    templates
                        .iter()
                        .map(|(name, on)| Json::Arr(vec![text(name), Json::Bool(*on)]))
                        .collect(),
                ),
            )],
        ),
        View::SetSavedTemplates(templates) => (
            "set_saved_templates",
            vec![field(
                "templates",
                Json::Arr(templates.iter().map(template_json).collect()),
            )],
        ),
        View::SetQueued(queued) => ("set_queued", vec![field("queued", texts(queued))]),
        View::SetOffers(offers, effort) => (
            "set_offers",
            vec![
                field(
                    "offers",
                    Json::Arr(offers.iter().map(offer_row_json).collect()),
                ),
                field("effort", text(effort)),
            ],
        ),
        View::SetNetworks(client) => (
            "set_networks",
            vec![field(
                "client",
                carried(client.as_ref(), client.to_json(), client_paths),
            )],
        ),
        View::SetPrompts(prompts) => (
            "set_prompts",
            vec![field(
                "prompts",
                Json::Arr(
                    prompts
                        .iter()
                        .map(|entry| {
                            Json::Obj(vec![
                                field("name", text(entry.name.word())),
                                field("text", text(&entry.text)),
                                field("custom", Json::Bool(entry.custom)),
                                field("trouble", optional(&entry.trouble)),
                            ])
                        })
                        .collect(),
                ),
            )],
        ),
        View::SetAllAllowed(all_allowed) => (
            "set_all_allowed",
            vec![field(
                "conversations",
                Json::Arr(all_allowed.iter().map(|id| text(id)).collect()),
            )],
        ),
        View::SetModes(modes) => (
            "set_modes",
            vec![field(
                "modes",
                modes.as_ref().map_or(Json::Null, |modes| {
                    Json::Arr(
                        modes
                            .iter()
                            .map(|(key, mode)| Json::Arr(vec![text(key), text(mode.word())]))
                            .collect(),
                    )
                }),
            )],
        ),
        View::SetModels(default, contexts) => (
            "set_models",
            vec![
                field("default", text(default)),
                field(
                    "contexts",
                    Json::Arr(
                        contexts
                            .iter()
                            .map(|(model, context)| {
                                Json::Arr(vec![text(model), Json::from(*context)])
                            })
                            .collect(),
                    ),
                ),
            ],
        ),
        View::SetCredit(credit) => ("set_credit", vec![field("credit", optional(credit))]),
        View::SetClosing(id, doing) => (
            "set_closing",
            vec![
                field("id", id_json(id)),
                field("doing", doing.map_or(Json::Null, text)),
            ],
        ),
        View::KeySaved(path) => ("key_saved", vec![field("path", text(path))]),
        View::KeyExists => ("key_exists", Vec::new()),
        View::KeyRefused(why) => ("key_refused", vec![field("why", text(why))]),
        View::SetKeyPath(path) => ("set_key_path", vec![field("path", optional(path))]),
        View::SetKeyed(keyed) => ("set_keyed", vec![field("keyed", Json::Bool(*keyed))]),
        View::AskRemoval(id, title, lost, archive) => (
            "ask_removal",
            vec![
                field("id", id_json(id)),
                field("title", text(title)),
                field("lost", texts(lost)),
                field("archive", Json::Bool(*archive)),
            ],
        ),
        View::Ask(card) => ("ask", vec![field("card", card_json(card))]),
        View::Question(asked) => ("question", vec![field("asked", asked_json(asked))]),
        View::AskAdmission(template, remotes) => (
            "ask_admission",
            vec![
                field("template", text(template)),
                field("remotes", texts(remotes)),
            ],
        ),
        View::ShowWorkspace(id, record) => (
            "show_workspace",
            vec![
                field("id", id_json(id)),
                field("record", record_json(record)),
            ],
        ),
        View::ReshowWorkspace(id, record) => (
            "reshow_workspace",
            vec![
                field("id", id_json(id)),
                field("record", record_json(record)),
            ],
        ),
        View::ShowSkills(found) => ("show_skills", vec![field("found", found_json(found))]),
        View::ShowSchedules(entries) => {
            ("show_schedules", vec![field("entries", rows_json(entries))])
        }
        View::ShowCommands(commands, more) => (
            "show_commands",
            vec![
                field("commands", rows_json(commands)),
                field("more", Json::from(*more as u64)),
            ],
        ),
        View::ShowOutput(id, number, read) => (
            "show_output",
            vec![
                field("id", id_json(id)),
                field("number", Json::from(*number)),
                field("read", result_json(read, output_json)),
            ],
        ),
        View::ShowDebug(id, request, shown) => (
            "show_debug",
            vec![
                field("id", id_json(id)),
                field("request", Json::from(*request)),
                field("view", result_json(shown, |rows| rows_json(rows))),
            ],
        ),
        View::SetWithoutJev(without) => (
            "set_without_jev",
            vec![field("without", Json::Bool(*without))],
        ),
        View::SetToday(today) => ("set_today", vec![field("today", Json::from(*today))]),
        View::SetSubAgents(read) => (
            "set_sub_agents",
            vec![field(
                "read",
                Json::Arr(
                    read.iter()
                        .map(|(id, tasks)| {
                            Json::Arr(vec![
                                id_json(id),
                                Json::Arr(tasks.iter().map(task_json).collect()),
                            ])
                        })
                        .collect(),
                ),
            )],
        ),
        View::SetRows(rows) => ("set_rows", vec![field("rows", row_list_json(rows))]),
        View::AddRow(row) => ("add_row", vec![field("row", row.to_json())]),
        View::RemoveRow(id) => ("remove_row", vec![field("id", id_json(id))]),
        View::SetArchived(id, archived) => (
            "set_archived",
            vec![
                field("id", id_json(id)),
                field("archived", Json::Bool(*archived)),
            ],
        ),
        View::SetNoWorkspaces(why) => ("set_no_workspaces", vec![field("why", optional(why))]),
        View::SetJailless(why) => ("set_jailless", vec![field("why", optional(why))]),
        View::SetLimits(limits) => (
            "set_limits",
            vec![
                field("turn", limits.turn.map_or(Json::Null, Json::from)),
                field(
                    "conversation",
                    limits.conversation.map_or(Json::Null, Json::from),
                ),
                field("day", limits.day.map_or(Json::Null, Json::from)),
            ],
        ),
        View::SetDefaultRouting(routing) => {
            ("set_default_routing", vec![field("routing", text(routing))])
        }
        View::SetDefaultModel(model) => ("set_default_model", vec![field("model", text(model))]),
        View::OpenNewTemplate(remote) => {
            ("open_new_template", vec![field("remote", optional(remote))])
        }
        View::FocusComposer => ("focus_composer", Vec::new()),
        View::Bye(why) => ("bye", vec![field("why", text(why))]),
        View::Folders(answer) => (
            "folders",
            vec![
                field("asked", paths_json(&answer.asked)),
                field("from", optional(&answer.from)),
                field("read", result_json(&answer.read, folders_json)),
            ],
        ),
    };
    typed(kind, pairs)
}

/// One message from the session to the window, back. It is refused for any
/// fault, a word the session never says for a closing conversation included.
/// The conversation a message is of, when one was open.
fn of_from(value: &Json) -> Result<Option<Id>, String> {
    match value.get("of") {
        Some(Json::Null) => Ok(None),
        Some(_) => id_from(value, "of").map(Some),
        None => Err("no conversation it is of".into()),
    }
}

pub fn decode_view(bytes: &[u8]) -> Result<View, String> {
    let value = frame(bytes)?;
    let value = &value;
    Ok(match value.get("type").and_then(Json::as_str) {
        Some("opened") => View::Opened(Box::new(member(value, "book", Book::from_json)?)),
        Some("set_mode") => View::SetMode(
            value
                .get("mode")
                .and_then(Json::as_str)
                .and_then(Mode::parse)
                .ok_or("no mode")?,
        ),
        Some("counted") => View::Counted(
            id_from(value, "id")?,
            items(value, "events", Event::from_json)?,
            numbers(value, "ended")?,
        ),
        Some("set_share") => View::SetShare(match value.get("share") {
            Some(Json::Null) => None,
            Some(share) => match share.as_arr() {
                Some([first, total]) => Some((
                    first
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or("a share out of range")?,
                    total
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or("a share out of range")?,
                )),
                _ => return Err("a share that is not two numbers".into()),
            },
            None => return Err("no share".into()),
        }),
        Some("note") => View::Note(string(value, "message")?),
        Some("withdraw") => View::Withdraw(
            id_from(value, "conversation")?,
            maybe_number(value, "call")?,
        ),
        Some("set_active") => View::SetActive(id_from(value, "id")?),
        Some("update") => View::Update(
            of_from(value)?,
            member(value, "update", update_from)?,
            number(value, "now")?,
            pictures_from(value)?,
        ),
        Some("background") => View::Background(
            id_from(value, "id")?,
            member(value, "update", update_from)?,
            number(value, "now")?,
        ),
        Some("replay") => View::Replay(
            of_from(value)?,
            member(value, "prefix", |prefix| {
                result_from(prefix, |prefix| {
                    prefix
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| "a prefix that is not text".to_string())
                })
            })?,
            items(value, "events", Event::from_json)?,
            pictures_from(value)?,
        ),
        Some("give_back") => View::GiveBack(
            string(value, "text")?,
            attached_from(value, "pictures")?,
            string(value, "why")?,
        ),
        Some("restore") => View::Restore(string(value, "text")?, string(value, "why")?),
        Some("prompt_saved") => View::PromptSaved(ask_number(value)?, string(value, "said")?),
        Some("prompt_refused") => View::PromptRefused(ask_number(value)?, string(value, "why")?),
        Some("template_saved") => View::TemplateSaved(string(value, "said")?),
        Some("template_refused") => View::TemplateRefused(string(value, "why")?),
        Some("set_templates") => {
            View::SetTemplates(items(value, "templates", |one| match one.as_arr() {
                Some([Json::Str(name), Json::Bool(on)]) => Ok((name.clone(), *on)),
                _ => Err("a template that is not a name and a flag".to_string()),
            })?)
        }
        Some("set_saved_templates") => {
            View::SetSavedTemplates(items(value, "templates", template_from)?)
        }
        Some("set_queued") => View::SetQueued(strings(value, "queued")?),
        Some("set_offers") => View::SetOffers(
            items(value, "offers", offer_row_from)?,
            string(value, "effort")?,
        ),
        Some("set_networks") => View::SetNetworks(Box::new(member(value, "client", |client| {
            carried_from(client, Client::from_json, client_paths)
        })?)),
        Some("set_prompts") => View::SetPrompts(items(value, "prompts", |one| {
            Ok(crate::prompts::Entry {
                name: prompt_name(one)?,
                text: string(one, "text")?,
                custom: flag(one, "custom")?,
                trouble: maybe(one, "trouble")?,
            })
        })?),
        Some("set_all_allowed") => View::SetAllAllowed(strings(value, "conversations")?),
        Some("set_modes") => View::SetModes(match value.get("modes") {
            Some(Json::Null) => None,
            Some(_) => Some(items(value, "modes", |one| match one.as_arr() {
                Some([Json::Str(key), Json::Str(word)]) => {
                    Ok((key.clone(), Mode::parse(word).ok_or("no such mode")?))
                }
                _ => Err("a mode that is not a key and a word".to_string()),
            })?),
            None => return Err("no modes".into()),
        }),
        Some("set_models") => View::SetModels(
            string(value, "default")?,
            items(value, "contexts", |one| match one.as_arr() {
                Some([Json::Str(model), context]) => Ok((
                    model.clone(),
                    context.as_u64().ok_or("a context that is not a number")?,
                )),
                _ => Err("a context that is not a model and a number".to_string()),
            })?,
        ),
        Some("set_credit") => View::SetCredit(maybe(value, "credit")?),
        Some("set_closing") => View::SetClosing(
            id_from(value, "id")?,
            match maybe(value, "doing")? {
                Some(word) => Some(closing_from(&word).ok_or("a closing word nobody uses")?),
                None => None,
            },
        ),
        Some("key_saved") => View::KeySaved(string(value, "path")?),
        Some("key_exists") => View::KeyExists,
        Some("key_refused") => View::KeyRefused(string(value, "why")?),
        Some("set_key_path") => View::SetKeyPath(maybe(value, "path")?),
        Some("set_keyed") => View::SetKeyed(flag(value, "keyed")?),
        Some("ask_removal") => View::AskRemoval(
            id_from(value, "id")?,
            string(value, "title")?,
            strings(value, "lost")?,
            flag(value, "archive")?,
        ),
        Some("ask") => View::Ask(member(value, "card", card_from)?),
        Some("question") => View::Question(member(value, "asked", asked_from)?),
        Some("ask_admission") => {
            View::AskAdmission(string(value, "template")?, strings(value, "remotes")?)
        }
        Some("show_workspace") => View::ShowWorkspace(
            id_from(value, "id")?,
            Box::new(member(value, "record", record_from)?),
        ),
        Some("reshow_workspace") => View::ReshowWorkspace(
            id_from(value, "id")?,
            Box::new(member(value, "record", record_from)?),
        ),
        Some("show_skills") => View::ShowSkills(member(value, "found", found_from)?),
        Some("show_schedules") => View::ShowSchedules(member(value, "entries", rows_from)?),
        Some("show_commands") => {
            View::ShowCommands(member(value, "commands", rows_from)?, size(value, "more")?)
        }
        Some("show_output") => View::ShowOutput(
            id_from(value, "id")?,
            number(value, "number")?,
            member(value, "read", |read| result_from(read, output_from))?,
        ),
        Some("show_debug") => View::ShowDebug(
            id_from(value, "id")?,
            number(value, "request")?,
            member(value, "view", |shown| result_from(shown, rows_from))?,
        ),
        Some("set_without_jev") => View::SetWithoutJev(flag(value, "without")?),
        Some("set_today") => View::SetToday(number(value, "today")?),
        Some("set_sub_agents") => {
            View::SetSubAgents(items(value, "read", |one| match one.as_arr() {
                Some([Json::Str(id), Json::Arr(tasks)]) => Ok((
                    Id::parse(id).ok_or("a sub-agent's conversation is not an id")?,
                    tasks.iter().map(task_from).collect::<Result<_, _>>()?,
                )),
                _ => Err("sub-agents that are not an id and a list".to_string()),
            })?)
        }
        Some("set_rows") => View::SetRows(items(value, "rows", Row::from_json)?),
        Some("add_row") => View::AddRow(member(value, "row", Row::from_json)?),
        Some("remove_row") => View::RemoveRow(id_from(value, "id")?),
        Some("set_archived") => View::SetArchived(id_from(value, "id")?, flag(value, "archived")?),
        Some("set_no_workspaces") => View::SetNoWorkspaces(maybe(value, "why")?),
        Some("set_jailless") => View::SetJailless(maybe(value, "why")?),
        Some("set_limits") => View::SetLimits(Limits {
            turn: maybe_number(value, "turn")?,
            conversation: maybe_number(value, "conversation")?,
            day: maybe_number(value, "day")?,
        }),
        Some("set_default_routing") => View::SetDefaultRouting(string(value, "routing")?),
        Some("set_default_model") => View::SetDefaultModel(string(value, "model")?),
        Some("open_new_template") => View::OpenNewTemplate(maybe(value, "remote")?),
        Some("focus_composer") => View::FocusComposer,
        Some("bye") => View::Bye(string(value, "why")?),
        Some("folders") => View::Folders(FoldersAnswer {
            asked: items(value, "asked", path_from)?,
            from: maybe(value, "from")?,
            read: member(value, "read", |read| result_from(read, folders_from))?,
        }),
        _ => return Err("a view of an unknown type".into()),
    })
}

/// One message from the window to the session. A `SaveKey`'s frame holds the
/// key: it goes to the socket and nowhere else.
pub fn encode_request(request: &Request) -> Vec<u8> {
    let (kind, pairs): (&str, Pairs) = match request {
        Request::Open(id) => ("open", vec![field("id", id_json(id))]),
        Request::New => ("new", Vec::new()),
        Request::NewScratch(unconfined) => (
            "new_scratch",
            vec![field("unconfined", Json::Bool(*unconfined))],
        ),
        Request::NewIn(path, unconfined) => (
            "new_in",
            vec![
                field("path", path_json(path)),
                field("unconfined", Json::Bool(*unconfined)),
            ],
        ),
        Request::NewFrom(name) => ("new_from", vec![field("name", text(name))]),
        Request::SaveTemplate {
            template,
            replacing,
        } => (
            "save_template",
            vec![
                field("template", template_json(template)),
                field("replacing", optional(replacing)),
            ],
        ),
        Request::RemoveTemplate(name) => ("remove_template", vec![field("name", text(name))]),
        Request::Send(said, pictures) => (
            "send",
            vec![
                field("text", text(said)),
                field("pictures", attached_json(pictures)),
            ],
        ),
        Request::Retry => ("retry", Vec::new()),
        Request::Interrupt => ("interrupt", Vec::new()),
        Request::Pause(paused) => ("pause", vec![field("paused", Json::Bool(*paused))]),
        Request::ClearTodo => ("clear_todo", Vec::new()),
        Request::Compact(focus) => ("compact", vec![field("focus", optional(focus))]),
        Request::Review(revision) => ("review", vec![field("revision", text(revision))]),
        Request::Schedule(command) => ("schedule", vec![field("command", command_json(command))]),
        Request::Command {
            name,
            args,
            typed: said,
            pictures,
        } => (
            "command",
            vec![
                field("name", text(name)),
                field("args", text(args)),
                field("typed", text(said)),
                field("pictures", attached_json(pictures)),
            ],
        ),
        Request::ListFolders {
            candidates,
            or_home,
            from,
        } => (
            "list_folders",
            vec![
                field("candidates", paths_json(candidates)),
                field("or_home", Json::Bool(*or_home)),
                field("from", optional(from)),
            ],
        ),
        Request::Commands => ("commands", Vec::new()),
        Request::OpenLink(url) => ("open_link", vec![field("url", text(url))]),
        Request::Skills => ("skills", Vec::new()),
        Request::Resume {
            conversation,
            turn,
            choice,
        } => (
            "resume",
            vec![
                field("conversation", id_json(conversation)),
                field("turn", Json::from(*turn)),
                field("choice", text(choice.word())),
            ],
        ),
        Request::Redraw => ("redraw", Vec::new()),
        Request::Received => ("received", Vec::new()),
        Request::Claim { token, asked } => (
            "claim",
            vec![
                field("token", text(token)),
                field("asked", Json::Bool(*asked)),
            ],
        ),
        Request::SaveShare(first, total) => (
            "save_share",
            vec![
                field("first", Json::from(*first)),
                field("total", Json::from(*total)),
            ],
        ),
        Request::SaveKey { secret, replace } => (
            "save_key",
            vec![
                field("secret", text(secret.expose())),
                field("replace", Json::Bool(*replace)),
            ],
        ),
        Request::Export => ("export", Vec::new()),
        Request::ExportConversation(id) => ("export_conversation", vec![field("id", id_json(id))]),
        Request::Debug { id, request } => (
            "debug",
            vec![
                field("id", id_json(id)),
                field("request", Json::from(*request)),
            ],
        ),
        Request::Delete(id) => ("delete", vec![field("id", id_json(id))]),
        Request::Archive { id, archived } => (
            "archive",
            vec![
                field("id", id_json(id)),
                field("archived", Json::Bool(*archived)),
            ],
        ),
        Request::Kill { id, number } => (
            "kill",
            vec![
                field("id", id_json(id)),
                field("number", Json::from(*number)),
            ],
        ),
        Request::Output { id, number } => (
            "output",
            vec![
                field("id", id_json(id)),
                field("number", Json::from(*number)),
            ],
        ),
        Request::SetDefault(model) => ("set_default", vec![field("model", text(model))]),
        Request::SetPrompt {
            ask,
            name,
            from,
            text: said,
        } => (
            "set_prompt",
            vec![
                field("ask", Json::from(*ask)),
                field("name", text(name.word())),
                field("from", text(from)),
                field("text", text(said)),
            ],
        ),
        Request::ListPrompts => ("list_prompts", Vec::new()),
        Request::SetAllAllowed { conversation, on } => (
            "set_all_allowed",
            vec![
                field("conversation", id_json(conversation)),
                field("on", Json::Bool(*on)),
            ],
        ),
        Request::SetMode { conversation, mode } => (
            "set_mode",
            vec![
                field("conversation", id_json(conversation)),
                field("mode", text(mode.word())),
            ],
        ),
        Request::Trust {
            conversation,
            digest,
        } => (
            "trust",
            vec![
                field("conversation", id_json(conversation)),
                field("digest", optional(digest)),
            ],
        ),
        Request::Workspace(id) => ("workspace", vec![field("id", id_json(id))]),
        Request::Remove { id, remove } => (
            "remove",
            vec![
                field("id", id_json(id)),
                field("remove", Json::Bool(*remove)),
            ],
        ),
        Request::Admit { template, remotes } => (
            "admit",
            vec![
                field("template", text(template)),
                field("remotes", texts(remotes)),
            ],
        ),
        Request::Decide {
            conversation,
            call,
            answer,
        } => (
            "decide",
            vec![
                field("conversation", id_json(conversation)),
                field("call", Json::from(*call)),
                field("answer", answer_json(answer)),
            ],
        ),
        Request::Quit => ("quit", Vec::new()),
        Request::Choose { model, effort } => (
            "choose",
            vec![
                field("model", optional(model)),
                field("effort", optional(effort)),
            ],
        ),
        Request::Route { routing } => ("route", vec![field("routing", optional(routing))]),
    };
    typed(kind, pairs)
}

/// One message from the window to the session, back. A key is held to what
/// the key dialog takes, and its refusals name the fault, never the text.
/// What a window meant by "the open conversation" when it asked (`broker`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Showing {
    /// None was open.
    Nothing,
    Conversation(Id),
    /// Whichever comes of the new one it has asked for and not yet heard of: a
    /// message typed straight after asking is for that one. It goes on showing
    /// this one if none comes.
    New(Option<Id>),
}

/// A request with what the window was showing when it asked: the one a request
/// that names no conversation means.
pub fn encode_asked(showing: &Showing, request: &Request) -> Vec<u8> {
    let mut bytes = br#"{"of":"#.to_vec();
    match showing {
        Showing::Conversation(of) => {
            bytes.push(b'"');
            bytes.extend_from_slice(of.as_str().as_bytes());
            bytes.push(b'"');
        }
        Showing::Nothing => bytes.extend_from_slice(b"null"),
        Showing::New(prior) => {
            bytes.extend_from_slice(br#""new","prior":"#);
            match prior {
                Some(prior) => {
                    bytes.push(b'"');
                    bytes.extend_from_slice(prior.as_str().as_bytes());
                    bytes.push(b'"');
                }
                None => bytes.extend_from_slice(b"null"),
            }
        }
    }
    bytes.extend_from_slice(br#","request":"#);
    bytes.extend_from_slice(&encode_request(request));
    bytes.push(b'}');
    bytes
}

/// What `encode_asked` made.
pub fn decode_asked(bytes: &[u8]) -> Result<(Showing, Request), String> {
    let value = frame(bytes)?;
    let showing = match value.get("of") {
        Some(Json::Str(new)) if new == "new" => Showing::New(match value.get("prior") {
            Some(Json::Null) => None,
            Some(_) => Some(id_from(&value, "prior")?),
            None => return Err("a new one and no word of what it was asked from".into()),
        }),
        _ => match of_from(&value)? {
            Some(of) => Showing::Conversation(of),
            None => Showing::Nothing,
        },
    };
    let request = value.get("request").ok_or("no request")?;
    Ok((showing, request_from(request)?))
}

pub fn decode_request(bytes: &[u8]) -> Result<Request, String> {
    request_from(&frame(bytes)?)
}

fn request_from(value: &Json) -> Result<Request, String> {
    Ok(match value.get("type").and_then(Json::as_str) {
        Some("open") => Request::Open(id_from(value, "id")?),
        Some("new") => Request::New,
        Some("new_scratch") => Request::NewScratch(flag(value, "unconfined")?),
        Some("new_in") => Request::NewIn(
            member(value, "path", path_from)?,
            flag(value, "unconfined")?,
        ),
        Some("new_from") => Request::NewFrom(string(value, "name")?),
        Some("save_template") => Request::SaveTemplate {
            template: member(value, "template", template_from)?,
            replacing: maybe(value, "replacing")?,
        },
        Some("remove_template") => Request::RemoveTemplate(string(value, "name")?),
        Some("send") => Request::Send(string(value, "text")?, attached_from(value, "pictures")?),
        Some("retry") => Request::Retry,
        Some("interrupt") => Request::Interrupt,
        Some("pause") => Request::Pause(flag(value, "paused")?),
        Some("clear_todo") => Request::ClearTodo,
        Some("compact") => Request::Compact(maybe(value, "focus")?),
        Some("review") => Request::Review(string(value, "revision")?),
        Some("schedule") => Request::Schedule(member(value, "command", command_from)?),
        Some("command") => Request::Command {
            name: Some(string(value, "name")?)
                .filter(|name| crate::commands::name_ok(name))
                .ok_or("a command's name is not one")?,
            args: string(value, "args")?,
            typed: string(value, "typed")?,
            pictures: attached_from(value, "pictures")?,
        },
        Some("list_folders") => Request::ListFolders {
            candidates: items(value, "candidates", path_from)?,
            or_home: flag(value, "or_home")?,
            from: maybe(value, "from")?,
        },
        Some("commands") => Request::Commands,
        Some("open_link") => Request::OpenLink(string(value, "url")?),
        Some("skills") => Request::Skills,
        Some("resume") => Request::Resume {
            conversation: id_from(value, "conversation")?,
            turn: number(value, "turn")?,
            choice: Resumed::parse(&string(value, "choice")?).ok_or("no such choice")?,
        },
        Some("redraw") => Request::Redraw,
        Some("received") => Request::Received,
        Some("claim") => Request::Claim {
            token: string(value, "token")?,
            asked: flag(value, "asked")?,
        },
        Some("save_share") => Request::SaveShare(narrow(value, "first")?, narrow(value, "total")?),
        Some("save_key") => {
            let secret = string(value, "secret")?;
            crate::key::check_key(&secret)?;
            Request::SaveKey {
                secret: Secret::new(secret),
                replace: flag(value, "replace")?,
            }
        }
        Some("export") => Request::Export,
        Some("export_conversation") => Request::ExportConversation(id_from(value, "id")?),
        Some("debug") => Request::Debug {
            id: id_from(value, "id")?,
            request: number(value, "request")?,
        },
        Some("delete") => Request::Delete(id_from(value, "id")?),
        Some("archive") => Request::Archive {
            id: id_from(value, "id")?,
            archived: flag(value, "archived")?,
        },
        Some("kill") => Request::Kill {
            id: id_from(value, "id")?,
            number: number(value, "number")?,
        },
        Some("output") => Request::Output {
            id: id_from(value, "id")?,
            number: number(value, "number")?,
        },
        Some("set_default") => Request::SetDefault(string(value, "model")?),
        Some("set_prompt") => Request::SetPrompt {
            ask: ask_number(value)?,
            name: prompt_name(value)?,
            from: string(value, "from")?,
            text: string(value, "text")?,
        },
        Some("list_prompts") => Request::ListPrompts,
        Some("set_all_allowed") => Request::SetAllAllowed {
            conversation: id_from(value, "conversation")?,
            on: flag(value, "on")?,
        },
        Some("set_mode") => Request::SetMode {
            conversation: id_from(value, "conversation")?,
            mode: mode_from(value, "mode")?,
        },
        Some("trust") => Request::Trust {
            conversation: id_from(value, "conversation")?,
            digest: match maybe(value, "digest")? {
                Some(digest) if !crate::rules::digest_hex(&digest) => {
                    return Err("a trust mark is a SHA-256 in lowercase hex".into());
                }
                digest => digest,
            },
        },
        Some("workspace") => Request::Workspace(id_from(value, "id")?),
        Some("remove") => Request::Remove {
            id: id_from(value, "id")?,
            remove: flag(value, "remove")?,
        },
        Some("admit") => Request::Admit {
            template: string(value, "template")?,
            remotes: strings(value, "remotes")?,
        },
        Some("decide") => Request::Decide {
            conversation: id_from(value, "conversation")?,
            call: number(value, "call")?,
            answer: member(value, "answer", answer_from)?,
        },
        Some("quit") => Request::Quit,
        Some("choose") => Request::Choose {
            model: maybe(value, "model")?,
            effort: maybe(value, "effort")?,
        },
        Some("route") => Request::Route {
            routing: maybe(value, "routing")?,
        },
        _ => return Err("a request of an unknown type".into()),
    })
}

/// A prompt's name as a message gives it.
fn prompt_name(value: &Json) -> Result<crate::prompts::Name, String> {
    let word = string(value, "name")?;
    crate::prompts::Name::from_word(&word).ok_or_else(|| format!("no prompt is called {word}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use std::collections::BTreeSet;
    use std::ffi::OsStr;
    use std::path::Path;

    use super::*;
    use crate::book::RowState;
    use crate::config::{McpServer, Network, TemplateShared};
    use crate::repo::Instructions;
    use crate::rules::{Offer as Remembered, Read, Rule};
    use crate::store::{Effect, Kind};
    use crate::workspace::{Entry, Repositories, Shared};

    const SECRET: &str = "sk-or-v1-0123456789abcdefSECRET";
    /// What every text a sample holds may be, whatever JSON makes of it.
    const AWKWARD: &str = "tab\t \"q\" \\ nul\0 nl\n é \u{1F600} \u{202E} ";

    fn id(n: u8) -> Id {
        Id::parse(&format!("{n:032x}")).unwrap()
    }

    /// A path no text holds: its bytes are not UTF-8.
    fn not_utf8(name: &str) -> PathBuf {
        let mut bytes = b"/srv/\xff\xfe-".to_vec();
        bytes.extend_from_slice(name.as_bytes());
        PathBuf::from(OsString::from_vec(bytes))
    }

    fn reference(n: u8) -> Reference {
        Reference {
            digest: format!("{n:02x}").repeat(32),
            mime: crate::picture::PNG,
            width: 3,
            height: 2,
            bytes: 256,
            label: format!("Image {n}"),
        }
    }

    fn every_byte() -> Vec<u8> {
        (0..=255).collect()
    }

    fn attached() -> Vec<Attached> {
        vec![
            (reference(1), Arc::from(every_byte())),
            (reference(2), Arc::from(Vec::new())),
        ]
    }

    fn pictures() -> Pictures {
        let mut pictures = Pictures::new();
        pictures.insert("readable".into(), Ok(every_byte()));
        pictures.insert("empty".into(), Ok(Vec::new()));
        pictures.insert("gone".into(), Err("it could not be read".into()));
        pictures
    }

    fn event(seq: u64, kind: Kind) -> Event {
        Event {
            seq,
            time: u64::MAX,
            kind,
        }
    }

    fn events() -> Vec<Event> {
        vec![
            event(
                1,
                Kind::User {
                    delivery: "0123456789abcdef0123456789abcdef".into(),
                    text: AWKWARD.into(),
                    pictures: vec![reference(1)],
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
                Kind::ToolResult {
                    reply: 1,
                    id: "call".into(),
                    name: "patch".into(),
                    call: 2,
                    content: AWKWARD.into(),
                    error: true,
                    kept: Some("kept".into()),
                    digest: None,
                    digests: vec![("a".into(), None), ("b".into(), Some("d".into()))],
                    pictures: Vec::new(),
                },
            ),
            event(
                4,
                Kind::Fired {
                    delivery: "d".into(),
                    schedule: "s".into(),
                    author: Some(id(3)),
                    text: "t".into(),
                    skipped: None,
                },
            ),
        ]
    }

    fn ups() -> Vec<Up> {
        vec![
            Up::Hello {
                title: AWKWARD.into(),
                torn: Some(9),
                interrupted: vec![1, u64::MAX],
                paused: true,
                prefix: None,
                last: 7,
            },
            Up::Event(events().remove(0)),
            Up::Refused {
                delivery: "d".into(),
                reason: "r".into(),
            },
            Up::Ask {
                call: 1,
                title: "t".into(),
                details: vec!["d".into()],
                always: Some(offers().remove(0)),
            },
            Up::Ask {
                call: 2,
                title: "t".into(),
                details: Vec::new(),
                always: None,
            },
            Up::Waking,
        ]
    }

    fn offers() -> Vec<Remembered> {
        vec![
            Remembered::Rules(Always::checked(true, vec!["shell cargo test".into()]).unwrap()),
            Remembered::Crossing {
                op: Crossed::Message,
                to: id(4).to_string(),
            },
        ]
    }

    fn updates() -> Vec<Update> {
        let mut all: Vec<Update> = ups().into_iter().map(Update::Up).collect();
        all.extend([
            Update::Refused {
                text: Some(AWKWARD.into()),
                pictures: vec![reference(1), reference(2)],
                reason: "r".into(),
            },
            Update::Refused {
                text: None,
                pictures: Vec::new(),
                reason: String::new(),
            },
            Update::Restarting {
                reason: "gone".into(),
            },
            Update::Failed {
                reason: "gone".into(),
            },
            Update::Undeliverable {
                delivery: "d".into(),
                reason: "r".into(),
            },
        ]);
        all
    }

    fn entry(name: &str) -> Entry {
        Entry {
            remote: "https://github.com/timmydo/td".into(),
            base: "main".into(),
            branch: name.into(),
            sparse: Some(vec!["td-agent".into()]),
            store: not_utf8("store"),
            repository: PathBuf::from("/d/ws/one.git"),
            id: name.into(),
            checkout: not_utf8(name),
        }
    }

    fn workspaces() -> Vec<Workspace> {
        vec![
            Workspace::Scratch,
            Workspace::Directory(not_utf8("work")),
            Workspace::Directory(PathBuf::from("/plain/with \u{fffd} in it")),
            Workspace::Template("notes".into()),
            Workspace::Repositories(Repositories {
                template: "td".into(),
                name: "td-1-ab".into(),
                entries: vec![entry("agent"), entry("next")],
            }),
        ]
    }

    fn row(n: u8, workspace: Option<Workspace>) -> Row {
        Row {
            id: id(n),
            title: AWKWARD.into(),
            activity: u64::MAX - u64::from(n),
            state: match n % 3 {
                0 => RowState::Closed,
                1 => RowState::Running,
                _ => RowState::Failed,
            },
            paused: n.is_multiple_of(2),
            workspace,
            unconfined: n % 2 == 1,
            archived: n.is_multiple_of(3),
        }
    }

    fn rows() -> Vec<Row> {
        let mut all = vec![row(1, None)];
        all.extend(
            workspaces()
                .into_iter()
                .zip(2..)
                .map(|(workspace, n)| row(n, Some(workspace))),
        );
        all
    }

    /// A book as the session comes to hold one: a conversation open and
    /// running, one left running, and two being closed.
    fn book() -> Book {
        let mut book = Book::default();
        book.set_rows(rows());
        book.set_active(id(1));
        book.replay(&events());
        book.set_active(id(2));
        book.set_closing(&id(3), Some("deleting"));
        book.set_closing(&id(4), Some("archiving"));
        book.update(
            &Update::Up(Up::Event(event(
                7,
                Kind::Started {
                    effect: Effect::Turn,
                    of: 1,
                },
            ))),
            5,
        );
        book
    }

    fn mcp() -> Vec<McpServer> {
        vec![crate::config::mcp_server(
            "git",
            vec!["/bin/git".into(), "serve".into()],
            Some(vec!["status".into()]),
            Some(30),
        )
        .unwrap()]
    }

    fn templates() -> Vec<Template> {
        let plain = crate::config::checked_repo("/srv/td", "main", "a", None).unwrap();
        let sparse = crate::config::checked_repo(
            "https://github.com/timmydo/td",
            "main",
            "b",
            Some(vec!["td-agent".into(), "td-ui".into()]),
        )
        .unwrap();
        let mut shared = crate::config::shared_field("~/notes /srv/out:rw")
            .unwrap()
            .unwrap();
        shared.push(Shared {
            path: not_utf8("shared"),
            write: true,
        });
        vec![
            Template {
                name: "bare".into(),
                repos: Vec::new(),
                shared: None,
                network: None,
                system: false,
                mcp: Vec::new(),
            },
            Template {
                name: "whole".into(),
                repos: vec![plain, sparse],
                shared: Some(shared),
                network: Some(Network::Open),
                system: true,
                mcp: mcp(),
            },
        ]
    }

    fn clients() -> Vec<Client> {
        vec![
            Client::default(),
            // What `App::set_networks` reads: the policy, and each
            // template's name, system view and policy.
            Client {
                network: Network::Off,
                template_shared: vec![
                    TemplateShared {
                        name: "open".into(),
                        shared: None,
                        network: Some(Network::Open),
                        system: true,
                        mcp: mcp(),
                    },
                    TemplateShared {
                        name: "plain".into(),
                        shared: Some(vec![Shared {
                            path: not_utf8("own"),
                            write: false,
                        }]),
                        network: None,
                        system: false,
                        mcp: Vec::new(),
                    },
                ],
                shared: vec![Shared {
                    path: not_utf8("shared"),
                    write: true,
                }],
                compact_model: Some("a/b".into()),
                cold_resume_tokens: None,
                ..Client::default()
            },
        ]
    }

    fn records() -> Vec<Record> {
        let rule = Rule::parse("deny shell rm").unwrap();
        let found = Instructions::Found {
            name: "AGENTS.md".into(),
            text: AWKWARD.into(),
        };
        let commit = "0".repeat(40);
        let instructed = vec![
            Instructed {
                checkout: not_utf8("one"),
                base: commit.clone(),
                read: found.clone(),
                rules: Read::Found(vec![rule.clone()]),
            },
            Instructed {
                checkout: PathBuf::from("/plain/two"),
                base: commit,
                read: found,
                rules: Read::Found(vec![rule]),
            },
            Instructed {
                checkout: PathBuf::from("/plain/three"),
                base: "1".repeat(40),
                read: Instructions::Unread { why: "past".into() },
                rules: Read::unread("no"),
            },
            Instructed {
                checkout: PathBuf::from("/plain/four"),
                base: "2".repeat(64),
                read: Instructions::Absent,
                rules: Read::Absent,
            },
        ];
        vec![
            Record {
                instructions: Ok(instructed),
                prepared: Ok(vec![not_utf8("prepared"), PathBuf::from("relative")]),
                removed: true,
                trusted: Ok(Some("digest".into())),
            },
            Record {
                instructions: Ok(Vec::new()),
                prepared: Ok(Vec::new()),
                removed: false,
                trusted: Ok(None),
            },
            Record {
                instructions: Err("unread".into()),
                prepared: Err("unread".into()),
                removed: false,
                trusted: Err("unread".into()),
            },
        ]
    }

    /// One or more samples of every variant, `view_name` being what holds
    /// the list whole.
    fn view_samples() -> Vec<View> {
        let mut all = vec![
            View::Opened(Box::default()),
            View::Opened(Box::new(book())),
            View::SetMode(Mode::Ask),
            View::SetMode(Mode::Auto),
            View::Counted(id(1), events(), vec![u64::MAX]),
            View::Counted(id(2), Vec::new(), Vec::new()),
            View::SetShare(None),
            View::SetShare(Some((u32::MAX, 1))),
            View::Note(AWKWARD.into()),
            View::Withdraw(id(1), None),
            View::Withdraw(id(1), Some(u64::MAX)),
            View::SetActive(id(1)),
            View::Replay(Some(id(1)), Ok(AWKWARD.into()), events(), pictures()),
            View::Replay(None, Err("no prefix".into()), Vec::new(), Pictures::new()),
            View::GiveBack(AWKWARD.into(), attached(), "why".into()),
            View::GiveBack(String::new(), Vec::new(), String::new()),
            View::Restore(AWKWARD.into(), "why".into()),
            View::PromptSaved(7, "said".into()),
            View::PromptRefused(7, "why".into()),
            View::SetPrompts(vec![crate::prompts::Entry {
                name: crate::prompts::Name::Classifier,
                text: "broken".into(),
                custom: true,
                trouble: Some("why".into()),
            }]),
            View::TemplateSaved("said".into()),
            View::TemplateRefused("why".into()),
            View::SetTemplates(vec![("a".into(), true), ("b".into(), false)]),
            View::SetTemplates(Vec::new()),
            View::SetSavedTemplates(templates()),
            View::SetQueued(vec![AWKWARD.into(), String::new()]),
            View::SetOffers(
                vec![Offer {
                    id: "a/b".into(),
                    price: "$1".into(),
                    usable: true,
                    reasoning: false,
                }],
                "high".into(),
            ),
            View::SetPrompts(Vec::new()),
            View::SetPrompts(crate::prompts::Prompts::default().entries()),
            View::SetAllAllowed(Vec::new()),
            View::SetAllAllowed(vec!["id".into()]),
            View::SetModes(None),
            View::SetModes(Some(Vec::new())),
            View::SetModes(Some(vec![
                ("scratch".into(), Mode::Ask),
                ("workspace td".into(), Mode::Auto),
            ])),
            View::SetModels("a/b".into(), vec![("a/b".into(), u64::MAX)]),
            View::SetCredit(None),
            View::SetCredit(Some("1.5".into())),
            View::SetClosing(id(1), None),
            View::SetClosing(id(1), Some("deleting")),
            View::SetClosing(id(1), Some("archiving")),
            View::KeySaved("/k".into()),
            View::KeyExists,
            View::KeyRefused("why".into()),
            View::SetKeyPath(None),
            View::SetKeyPath(Some("/k".into())),
            View::SetKeyed(true),
            View::AskRemoval(id(1), AWKWARD.into(), vec!["lost".into()], true),
            View::AskRemoval(id(1), String::new(), Vec::new(), false),
            View::Question(Asked {
                conversation: id(1),
                call: 2,
                question: AWKWARD.into(),
                options: vec!["yes".into(), "no".into()],
            }),
            View::AskAdmission("td".into(), vec!["https://github.com/a/b".into()]),
            View::ShowSkills(Found {
                skills: vec![Skill {
                    name: "one".into(),
                    description: AWKWARD.into(),
                }],
                refused: vec![("two".into(), "why".into())],
                more: 3,
            }),
            View::ShowSkills(Found::default()),
            View::ShowSchedules(vec![("a".into(), AWKWARD.into())]),
            View::ShowCommands(vec![("a".into(), "b".into())], 4),
            View::ShowOutput(
                id(1),
                2,
                Ok(Output {
                    text: AWKWARD.into(),
                    from: 1,
                    to: 2,
                    start: 3,
                    total: u64::MAX,
                }),
            ),
            View::ShowOutput(id(1), 2, Err("why".into())),
            View::ShowDebug(id(1), 2, Ok(vec![("head".into(), AWKWARD.into())])),
            View::ShowDebug(id(1), 2, Err("why".into())),
            View::SetWithoutJev(true),
            View::SetToday(u64::MAX),
            View::SetSubAgents(vec![
                (
                    id(1),
                    vec![TaskStart {
                        seq: 1,
                        call: 2,
                        agent: "explore".into(),
                        prompt: AWKWARD.into(),
                    }],
                ),
                (id(2), Vec::new()),
            ]),
            View::SetRows(rows()),
            View::SetRows(Vec::new()),
            View::RemoveRow(id(1)),
            View::SetArchived(id(1), true),
            View::SetNoWorkspaces(None),
            View::SetNoWorkspaces(Some("why".into())),
            View::SetJailless(None),
            View::SetJailless(Some("why".into())),
            View::SetLimits(Limits {
                turn: None,
                conversation: Some(0),
                day: Some(u64::MAX),
            }),
            View::SetDefaultRouting("fast".into()),
            View::SetDefaultModel("a/b".into()),
            View::OpenNewTemplate(None),
            View::OpenNewTemplate(Some("https://github.com/a/b".into())),
            View::FocusComposer,
            View::Bye("why".into()),
            View::Folders(FoldersAnswer {
                asked: vec![not_utf8("asked"), PathBuf::from("/plain")],
                from: Some(AWKWARD.into()),
                read: Ok(Folders {
                    folder: not_utf8("folder"),
                    entries: vec![
                        ("plain".into(), String::new()),
                        (AWKWARD.into(), "git, link".into()),
                    ],
                    truncated: true,
                }),
            }),
            View::Folders(FoldersAnswer {
                asked: Vec::new(),
                from: None,
                read: Ok(Folders {
                    folder: PathBuf::from("/"),
                    entries: Vec::new(),
                    truncated: false,
                }),
            }),
            View::Folders(FoldersAnswer {
                asked: vec![PathBuf::from("/a//b")],
                from: None,
                read: Err(AWKWARD.into()),
            }),
        ];
        for update in updates() {
            all.push(View::Update(None, update.clone(), u64::MAX, pictures()));
            all.push(View::Update(
                Some(id(2)),
                update.clone(),
                0,
                Pictures::new(),
            ));
            all.push(View::Background(id(1), update, 0));
        }
        for client in clients() {
            all.push(View::SetNetworks(Box::new(client)));
        }
        for row in rows() {
            all.push(View::AddRow(row));
        }
        for record in records() {
            all.push(View::ShowWorkspace(id(1), Box::new(record.clone())));
            all.push(View::ReshowWorkspace(id(2), Box::new(record)));
        }
        for always in offers().into_iter().map(Some).chain([None]) {
            all.push(View::Ask(Card {
                conversation: id(1),
                call: u64::MAX,
                title: AWKWARD.into(),
                details: vec!["d".into()],
                resume: always.is_none(),
                always,
            }));
        }
        all
    }

    fn request_samples() -> Vec<Request> {
        let mut all = vec![
            Request::Open(id(1)),
            Request::New,
            Request::NewScratch(true),
            Request::ListFolders {
                candidates: vec![not_utf8("folder"), PathBuf::from("/a//b")],
                or_home: true,
                from: Some(AWKWARD.into()),
            },
            Request::ListFolders {
                candidates: Vec::new(),
                or_home: false,
                from: None,
            },
            // A relative one crosses, for the broker to refuse by name.
            Request::ListFolders {
                candidates: vec![PathBuf::from("relative")],
                or_home: false,
                from: None,
            },
            Request::NewIn(not_utf8("folder"), true),
            Request::NewIn(PathBuf::from("/plain"), false),
            Request::NewFrom("td".into()),
            Request::RemoveTemplate("td".into()),
            Request::Send(AWKWARD.into(), attached()),
            Request::Send(String::new(), Vec::new()),
            Request::Retry,
            Request::Interrupt,
            Request::Pause(true),
            Request::ClearTodo,
            Request::Compact(None),
            Request::Compact(Some(AWKWARD.into())),
            Request::Review("HEAD~1".into()),
            Request::Schedule(Command::Add {
                when: "at 9".into(),
                text: AWKWARD.into(),
                catch_up: true,
            }),
            Request::Schedule(Command::List),
            Request::Schedule(Command::Remove("s1".into())),
            Request::Command {
                name: "go".into(),
                args: "now".into(),
                typed: "/go now".into(),
                pictures: attached(),
            },
            Request::Commands,
            Request::OpenLink("https://example.org/?a=b".into()),
            Request::Skills,
            Request::Resume {
                conversation: id(1),
                turn: 3,
                choice: Resumed::Resend,
            },
            Request::Resume {
                conversation: id(1),
                turn: 3,
                choice: Resumed::Compact,
            },
            Request::Resume {
                conversation: id(1),
                turn: 3,
                choice: Resumed::Stop,
            },
            Request::Redraw,
            Request::Received,
            Request::Claim {
                token: "t".into(),
                asked: true,
            },
            Request::SaveShare(u32::MAX, 0),
            Request::SaveKey {
                secret: Secret::new(SECRET.into()),
                replace: true,
            },
            Request::Export,
            Request::ExportConversation(id(1)),
            Request::Debug {
                id: id(1),
                request: u64::MAX,
            },
            Request::Delete(id(1)),
            Request::Archive {
                id: id(1),
                archived: true,
            },
            Request::Kill {
                id: id(1),
                number: 2,
            },
            Request::Output {
                id: id(1),
                number: 2,
            },
            Request::SetDefault("a/b".into()),
            Request::SetPrompt {
                ask: 7,
                name: crate::prompts::Name::JevRequest,
                from: "c".into(),
                text: "a\nb".into(),
            },
            Request::ListPrompts,
            Request::SetAllAllowed {
                conversation: id(1),
                on: true,
            },
            Request::SetMode {
                conversation: id(1),
                mode: Mode::Ask,
            },
            Request::SetMode {
                conversation: id(1),
                mode: Mode::Auto,
            },
            Request::Trust {
                conversation: id(1),
                digest: None,
            },
            Request::Trust {
                conversation: id(1),
                digest: Some("d".repeat(64)),
            },
            Request::Workspace(id(1)),
            Request::Remove {
                id: id(1),
                remove: true,
            },
            Request::Admit {
                template: "td".into(),
                remotes: vec!["https://github.com/a/b".into()],
            },
            Request::Quit,
            Request::Choose {
                model: None,
                effort: None,
            },
            Request::Choose {
                model: Some("a/b".into()),
                effort: Some("high".into()),
            },
            Request::Route { routing: None },
            Request::Route {
                routing: Some("fast".into()),
            },
        ];
        for template in templates() {
            all.push(Request::SaveTemplate {
                template: template.clone(),
                replacing: None,
            });
            all.push(Request::SaveTemplate {
                template,
                replacing: Some("old".into()),
            });
        }
        for answer in [
            Answer::Once(false),
            Answer::AllowAll,
            Answer::Always {
                allow: true,
                everywhere: true,
                bodies: vec!["shell cargo test".into()],
            },
            Answer::Crossing {
                allow: false,
                op: Crossed::Read,
                to: id(4).to_string(),
            },
        ] {
            all.push(Request::Decide {
                conversation: id(1),
                call: 2,
                answer,
            });
        }
        all
    }

    /// Every variant, by name: no `_` arm, so a new variant does not compile
    /// until it is named here, and the test below finds no sample of it.
    fn view_name(view: &View) -> &'static str {
        match view {
            View::Opened(..) => "Opened",
            View::SetMode(..) => "SetMode",
            View::Counted(..) => "Counted",
            View::SetShare(..) => "SetShare",
            View::Note(..) => "Note",
            View::Withdraw(..) => "Withdraw",
            View::SetActive(..) => "SetActive",
            View::Update(..) => "Update",
            View::Background(..) => "Background",
            View::Replay(..) => "Replay",
            View::GiveBack(..) => "GiveBack",
            View::Restore(..) => "Restore",
            View::PromptSaved(..) => "PromptSaved",
            View::PromptRefused(..) => "PromptRefused",
            View::TemplateSaved(..) => "TemplateSaved",
            View::TemplateRefused(..) => "TemplateRefused",
            View::SetTemplates(..) => "SetTemplates",
            View::SetSavedTemplates(..) => "SetSavedTemplates",
            View::SetQueued(..) => "SetQueued",
            View::SetOffers(..) => "SetOffers",
            View::SetNetworks(..) => "SetNetworks",
            View::SetPrompts(..) => "SetPrompts",
            View::SetAllAllowed(..) => "SetAllAllowed",
            View::SetModes(..) => "SetModes",
            View::SetModels(..) => "SetModels",
            View::SetCredit(..) => "SetCredit",
            View::SetClosing(..) => "SetClosing",
            View::KeySaved(..) => "KeySaved",
            View::KeyExists => "KeyExists",
            View::KeyRefused(..) => "KeyRefused",
            View::SetKeyPath(..) => "SetKeyPath",
            View::SetKeyed(..) => "SetKeyed",
            View::AskRemoval(..) => "AskRemoval",
            View::Ask(..) => "Ask",
            View::Question(..) => "Question",
            View::AskAdmission(..) => "AskAdmission",
            View::ShowWorkspace(..) => "ShowWorkspace",
            View::ReshowWorkspace(..) => "ReshowWorkspace",
            View::ShowSkills(..) => "ShowSkills",
            View::ShowSchedules(..) => "ShowSchedules",
            View::ShowCommands(..) => "ShowCommands",
            View::ShowOutput(..) => "ShowOutput",
            View::ShowDebug(..) => "ShowDebug",
            View::SetWithoutJev(..) => "SetWithoutJev",
            View::SetToday(..) => "SetToday",
            View::SetSubAgents(..) => "SetSubAgents",
            View::SetRows(..) => "SetRows",
            View::AddRow(..) => "AddRow",
            View::RemoveRow(..) => "RemoveRow",
            View::SetArchived(..) => "SetArchived",
            View::SetNoWorkspaces(..) => "SetNoWorkspaces",
            View::SetJailless(..) => "SetJailless",
            View::SetLimits(..) => "SetLimits",
            View::SetDefaultRouting(..) => "SetDefaultRouting",
            View::SetDefaultModel(..) => "SetDefaultModel",
            View::OpenNewTemplate(..) => "OpenNewTemplate",
            View::FocusComposer => "FocusComposer",
            View::Bye(_) => "Bye",
            View::Folders(..) => "Folders",
        }
    }

    /// As `view_name`.
    fn request_name(request: &Request) -> &'static str {
        match request {
            Request::Open(..) => "Open",
            Request::New => "New",
            Request::NewScratch(..) => "NewScratch",
            Request::NewIn(..) => "NewIn",
            Request::NewFrom(..) => "NewFrom",
            Request::SaveTemplate { .. } => "SaveTemplate",
            Request::RemoveTemplate(..) => "RemoveTemplate",
            Request::Send(..) => "Send",
            Request::Retry => "Retry",
            Request::Interrupt => "Interrupt",
            Request::Pause(..) => "Pause",
            Request::ClearTodo => "ClearTodo",
            Request::Compact(..) => "Compact",
            Request::Review(..) => "Review",
            Request::Schedule(..) => "Schedule",
            Request::Command { .. } => "Command",
            Request::ListFolders { .. } => "ListFolders",
            Request::Commands => "Commands",
            Request::OpenLink(..) => "OpenLink",
            Request::Skills => "Skills",
            Request::Resume { .. } => "Resume",
            Request::Redraw => "Redraw",
            Request::Received => "Received",
            Request::Claim { .. } => "Claim",
            Request::SaveShare(..) => "SaveShare",
            Request::SaveKey { .. } => "SaveKey",
            Request::Export => "Export",
            Request::ExportConversation(..) => "ExportConversation",
            Request::Debug { .. } => "Debug",
            Request::Delete(..) => "Delete",
            Request::Archive { .. } => "Archive",
            Request::Kill { .. } => "Kill",
            Request::Output { .. } => "Output",
            Request::SetDefault(..) => "SetDefault",
            Request::SetPrompt { .. } => "SetPrompt",
            Request::ListPrompts => "ListPrompts",
            Request::SetAllAllowed { .. } => "SetAllAllowed",
            Request::SetMode { .. } => "SetMode",
            Request::Trust { .. } => "Trust",
            Request::Workspace(..) => "Workspace",
            Request::Remove { .. } => "Remove",
            Request::Admit { .. } => "Admit",
            Request::Decide { .. } => "Decide",
            Request::Quit => "Quit",
            Request::Choose { .. } => "Choose",
            Request::Route { .. } => "Route",
        }
    }

    /// The arms of the function `signature` opens in this file.
    fn arms(signature: &str) -> usize {
        let source = include_str!("window_wire.rs");
        let at = source.find(signature).unwrap();
        let body = &source[at..];
        let end = body.find("\n    }\n").unwrap();
        body[..end].lines().filter(|l| l.contains("=> \"")).count()
    }

    /// Each sample comes back the same, and encodes to the same bytes again.
    fn round_trip_view(view: &View) {
        let bytes = encode_view(view);
        let back = decode_view(&bytes).unwrap_or_else(|why| panic!("{view:?}: {why}"));
        assert_eq!(&back, view);
        assert_eq!(encode_view(&back), bytes, "{view:?}");
    }

    /// Neither goes on to its file or path unless it is what that file or path
    /// holds.
    #[test]
    fn a_command_name_or_a_trust_mark_that_would_lead_elsewhere_is_refused() {
        for name in ["../outside", "/etc/passwd", "a/b", ""] {
            let frame = encode_request(&Request::Command {
                name: name.into(),
                args: String::new(),
                typed: String::new(),
                pictures: Vec::new(),
            });
            assert!(decode_request(&frame).is_err(), "{name}");
        }
        for digest in [
            String::new(),
            "d".repeat(63),
            "D".repeat(64),
            format!("{}\nallow shell rm -rf /", "d".repeat(64)),
        ] {
            let frame = encode_request(&Request::Trust {
                conversation: id(1),
                digest: Some(digest.clone()),
            });
            assert!(decode_request(&frame).is_err(), "{digest}");
        }
    }

    #[test]
    fn a_request_says_what_the_window_was_showing() {
        for showing in [
            Showing::Nothing,
            Showing::Conversation(id(1)),
            Showing::New(None),
            Showing::New(Some(id(3))),
        ] {
            for request in [Request::Retry, Request::Open(id(2))] {
                let frame = encode_asked(&showing, &request);
                assert_eq!(decode_asked(&frame).unwrap(), (showing.clone(), request));
            }
        }
        for frame in [
            r#"{"request":{"type":"retry"}}"#.to_string(),
            r#"{"of":null}"#.to_string(),
            r#"{"of":"x","request":{"type":"retry"}}"#.to_string(),
            r#"{"of":"new","request":{"type":"retry"}}"#.to_string(),
            r#"{"of":"new","prior":"x","request":{"type":"retry"}}"#.to_string(),
            r#"{"of":null,"request":{"type":"never"}}"#.to_string(),
        ] {
            assert!(decode_asked(frame.as_bytes()).is_err(), "{frame}");
        }
    }

    fn round_trip_request(request: &Request) {
        let bytes = encode_request(request);
        let back = decode_request(&bytes).unwrap_or_else(|why| panic!("{request:?}: {why}"));
        assert_eq!(&back, request);
        assert_eq!(encode_request(&back), bytes, "{request:?}");
    }

    #[test]
    fn every_view_variant_has_a_sample_that_comes_back_the_same() {
        let samples = view_samples();
        let names: BTreeSet<_> = samples.iter().map(view_name).collect();
        assert_eq!(names.len(), arms("fn view_name(view: &View)"));
        samples.iter().for_each(round_trip_view);
    }

    #[test]
    fn every_request_variant_has_a_sample_that_comes_back_the_same() {
        let samples = request_samples();
        let names: BTreeSet<_> = samples.iter().map(request_name).collect();
        assert_eq!(names.len(), arms("fn request_name(request: &Request)"));
        samples.iter().for_each(round_trip_request);
    }

    /// `PathBuf`'s `==` reads components, so a path's bytes are compared here.
    fn bytes_of(path: &Path) -> &[u8] {
        path.as_os_str().as_bytes()
    }

    #[test]
    fn a_path_that_is_not_utf8_comes_back_byte_for_byte_wherever_it_is() {
        let path = not_utf8("folder");
        let bytes = encode_request(&Request::NewIn(path.clone(), false));
        let Request::NewIn(back, _) = decode_request(&bytes).unwrap() else {
            panic!("another request");
        };
        assert_eq!(bytes_of(&back), bytes_of(&path));
        // Two slashes are the same path to `==`, and not to these bytes.
        let doubled = PathBuf::from("/a//b");
        let bytes = encode_request(&Request::NewIn(doubled.clone(), false));
        let Request::NewIn(back, _) = decode_request(&bytes).unwrap() else {
            panic!("another request");
        };
        assert_eq!(back.as_os_str(), OsStr::new("/a//b"));

        // A form that writes text: the row's workspace, every path of it.
        let row = row(
            1,
            Some(Workspace::Repositories(Repositories {
                template: "td".into(),
                name: "td-1-ab".into(),
                entries: vec![entry("agent")],
            })),
        );
        let View::AddRow(back) = decode_view(&encode_view(&View::AddRow(row))).unwrap() else {
            panic!("another view");
        };
        let Some(Workspace::Repositories(back)) = back.workspace else {
            panic!("another workspace");
        };
        let one = back.entries.first().unwrap();
        assert_eq!(bytes_of(&one.store), bytes_of(&not_utf8("store")));
        assert_eq!(bytes_of(&one.checkout), bytes_of(&not_utf8("agent")));
        assert_eq!(bytes_of(&one.repository), b"/d/ws/one.git");
    }

    /// What was on the wire of a form is what it would write anywhere else:
    /// a path that needs none goes with no `paths` beside it.
    #[test]
    fn a_form_is_carried_unchanged() {
        let workspace = Workspace::Directory(PathBuf::from("/plain"));
        assert_eq!(
            workspace_json(&workspace).to_string(),
            format!("{{\"form\":{}}}", workspace.to_json())
        );
        let client = Client::default();
        let bytes = encode_view(&View::SetNetworks(Box::new(client.clone())));
        let value = td_json::parse_slice(&bytes).unwrap();
        assert_eq!(value.get_path(&["client", "form"]), Some(&client.to_json()));
        // An exact path must be the path the form says.
        let lie = Json::Obj(vec![
            field("form", workspace.to_json()),
            field("paths", Json::Arr(vec![path_json(&not_utf8("else"))])),
        ]);
        assert!(workspace_from_json(&lie).is_err());
        let extra = Json::Obj(vec![
            field("form", workspace.to_json()),
            field("paths", Json::Arr(vec![path_json(Path::new("/plain")); 2])),
        ]);
        assert!(workspace_from_json(&extra).is_err());
        let relative = Json::Obj(vec![field(
            "form",
            Json::Obj(vec![
                field("kind", text("directory")),
                field("path", text("relative")),
            ]),
        )]);
        assert!(workspace_from_json(&relative).is_err());
    }

    /// The words `App` gets are `window`'s, and a book's and a message's
    /// closing words are the same ones, by the same static.
    #[test]
    fn a_closing_word_is_one_the_session_says() {
        let window = include_str!("broker.rs");
        assert!(window.contains("let doing = if archive { \"archiving\" } else { \"deleting\" };"));
        let word = |frame: &str| decode_view(frame.as_bytes());
        let conversation = id(1);
        for said in ["archiving", "deleting"] {
            let frame =
                format!(r#"{{"type":"set_closing","id":"{conversation}","doing":"{said}"}}"#);
            assert_eq!(
                word(&frame).unwrap(),
                View::SetClosing(conversation.clone(), closing_from(said))
            );
        }
        for said in ["closing", "archived", "Deleting", "deleting ", ""] {
            let frame =
                format!(r#"{{"type":"set_closing","id":"{conversation}","doing":"{said}"}}"#);
            assert!(word(&frame).is_err(), "{said:?}");
            let book = format!(
                r#"{{"type":"opened","book":{{"rows":[],"active":null,"closing":[["{conversation}","{said}"]],"background_turns":[],"turns":[],"seen":0}}}}"#
            );
            assert!(word(&book).is_err(), "{said:?}");
        }
    }

    /// A book holds what its reducer made of the messages, whatever order
    /// that left, and the reducer goes on from there as the original does.
    #[test]
    fn a_book_comes_back_whole_and_goes_on_the_same() {
        let mut held = book();
        let mut sent =
            Book::from_json(&td_json::parse(&held.to_json().to_string()).unwrap()).unwrap();
        assert_eq!(sent, held);
        assert_eq!(sent.active(), Some(&id(2)));
        assert_eq!(sent.closing_word(&id(3)), Some("deleting"));
        assert_eq!(sent.closing_word(&id(4)), Some("archiving"));
        let end = Update::Up(Up::Event(event(
            8,
            Kind::Finished {
                started: 7,
                outcome: "replied".into(),
                retry: false,
            },
        )));
        held.update(&end, 9);
        sent.update(&end, 9);
        held.set_active(id(5));
        sent.set_active(id(5));
        assert_eq!(sent, held);
        // A row's state goes as such, not as the list names it.
        let mut one = row(1, None);
        one.state = RowState::Idle;
        one.paused = true;
        one.archived = true;
        assert_eq!(Row::from_json(&one.to_json()).unwrap(), one);
    }

    /// `set_networks` reads the policy and each template's name, system view
    /// and policy, and the existing forms' words carry them all.
    #[test]
    fn the_networks_read_the_same_after_crossing() {
        for client in clients() {
            let bytes = encode_view(&View::SetNetworks(Box::new(client.clone())));
            let View::SetNetworks(back) = decode_view(&bytes).unwrap() else {
                panic!("another view");
            };
            assert_eq!(back.network, client.network);
            assert_eq!(back.template_shared.len(), client.template_shared.len());
            for (one, other) in back.template_shared.iter().zip(&client.template_shared) {
                assert_eq!(
                    (&one.name, one.system, one.network),
                    (&other.name, other.system, other.network)
                );
            }
        }
    }

    fn fields(bytes: &[u8]) -> Pairs {
        match td_json::parse_slice(bytes).unwrap() {
            Json::Obj(fields) => fields,
            other => panic!("{other:?}"),
        }
    }

    fn frames() -> Vec<(&'static str, Vec<u8>)> {
        let views = view_samples().into_iter();
        let requests = request_samples().into_iter();
        views
            .map(|view| ("view", encode_view(&view)))
            .chain(requests.map(|request| ("request", encode_request(&request))))
            .collect()
    }

    fn refused(side: &str, bytes: &[u8]) -> bool {
        if side == "view" {
            decode_view(bytes).is_err()
        } else {
            decode_request(bytes).is_err()
        }
    }

    #[test]
    fn what_is_not_a_message_is_refused() {
        for bad in [
            &b""[..],
            b"not json",
            b"\xff\xfe",
            b"[]",
            b"null",
            b"7",
            b"{}",
            b"{\"type\":7}",
            b"{\"type\":null}",
            b"{\"type\":\"no such message\"}",
            b"{\"type\":\"open\"}{\"type\":\"new\"}",
            b"{\"type\":\"new\",\"type\":\"new\"}",
        ] {
            assert!(decode_view(bad).is_err(), "{bad:?}");
            assert!(decode_request(bad).is_err(), "{bad:?}");
        }
        // One side's words are not the other's.
        assert!(decode_view(b"{\"type\":\"quit\"}").is_err());
        assert!(decode_request(b"{\"type\":\"focus_composer\"}").is_err());
    }

    /// Every sample's every member is required, an empty one being null or
    /// an empty list, and holds what it should.
    #[test]
    fn each_field_missing_or_of_the_wrong_type_is_refused() {
        for (side, bytes) in frames() {
            let all = fields(&bytes);
            for at in 1..all.len() {
                let name = &all[at].0;
                let mut without = all.clone();
                without.remove(at);
                let frame = Json::Obj(without).to_string();
                assert!(refused(side, frame.as_bytes()), "without {name}: {frame}");
                for wrong in [
                    Json::Obj(vec![field("wrong", Json::Null)]),
                    Json::Arr(vec![Json::Arr(vec![Json::Null])]),
                ] {
                    let mut changed = all.clone();
                    changed[at].1 = wrong;
                    let frame = Json::Obj(changed).to_string();
                    assert!(refused(side, frame.as_bytes()), "{name}: {frame}");
                }
            }
        }
    }

    /// No fault of a frame panics: every cut of one, and its bytes changed,
    /// which may still read as something.
    #[test]
    fn a_frame_cut_or_changed_does_not_panic() {
        for (side, bytes) in frames() {
            if bytes.len() > 4096 {
                continue;
            }
            for cut in 0..bytes.len() {
                assert!(refused(side, &bytes[..cut]), "{:?}", &bytes[..cut]);
            }
            let mut changed = bytes.clone();
            for at in 0..changed.len() {
                let was = changed[at];
                for to in [b'"', b'0', b'{', b']', 0xff] {
                    changed[at] = to;
                    let _ = refused(side, &changed);
                }
                changed[at] = was;
            }
        }
    }

    #[test]
    fn numbers_past_their_type_and_values_out_of_their_set_are_refused() {
        let first = id(1);
        for frame in [
            r#"{"type":"save_share","first":4294967296,"total":1}"#.to_string(),
            r#"{"type":"save_share","first":-1,"total":1}"#.to_string(),
            r#"{"type":"save_share","first":1.5,"total":1}"#.to_string(),
            r#"{"type":"debug","id":"x","request":1}"#.to_string(),
            format!(r#"{{"type":"debug","id":"{first}","request":18446744073709551616}}"#),
            format!(r#"{{"type":"set_mode","conversation":"{first}","mode":"never"}}"#),
            format!(r#"{{"type":"resume","conversation":"{first}","turn":1,"choice":"never"}}"#),
            r#"{"type":"new_in","path":"not base64!","unconfined":true}"#.to_string(),
            r#"{"type":"new_in","path":7,"unconfined":true}"#.to_string(),
            r#"{"type":"list_folders","candidates":["not base64!"],"or_home":true,"from":null}"#
                .to_string(),
            r#"{"type":"list_folders","candidates":[7],"or_home":true,"from":null}"#.to_string(),
            r#"{"type":"list_folders","candidates":"Lw==","or_home":true,"from":null}"#.to_string(),
            r#"{"type":"list_folders","candidates":[],"or_home":"yes","from":null}"#.to_string(),
            r#"{"type":"list_folders","candidates":[],"or_home":true,"from":7}"#.to_string(),
            format!(
                r#"{{"type":"decide","conversation":"{first}","call":1,"answer":{{"answer":"always","allow":true,"everywhere":false,"bodies":["shell a\nallow shell b"]}}}}"#
            ),
            format!(
                r#"{{"type":"decide","conversation":"{first}","call":1,"answer":{{"answer":"crossing","allow":true,"op":"read","to":"not an id"}}}}"#
            ),
            format!(
                r#"{{"type":"decide","conversation":"{first}","call":1,"answer":{{"answer":"never","allow":true}}}}"#
            ),
        ] {
            assert!(decode_request(frame.as_bytes()).is_err(), "{frame}");
        }
        for frame in [
            r#"{"type":"set_share","share":[1]}"#.to_string(),
            r#"{"type":"set_share","share":[1,4294967296]}"#.to_string(),
            r#"{"type":"set_modes","modes":[["k","never"]]}"#.to_string(),
            r#"{"type":"show_commands","commands":[],"more":-1}"#.to_string(),
            r#"{"type":"update","update":{"type":"nothing"},"now":1,"pictures":[]}"#.to_string(),
            r#"{"type":"update","update":{"type":"failed","reason":"r"},"now":1,"pictures":[{"digest":"d","read":{"ok":"AA==","err":"both"}}]}"#.to_string(),
            r#"{"type":"update","update":{"type":"failed","reason":"r"},"now":1,"pictures":[{"digest":"d","read":{}}]}"#.to_string(),
            r#"{"type":"update","update":{"type":"failed","reason":"r"},"now":1,"pictures":[{"digest":"d","read":{"ok":"!"}}]}"#.to_string(),
            r#"{"type":"update","update":{"type":"failed","reason":"r"},"now":1,"pictures":[{"digest":"d","read":{"err":"a"}},{"digest":"d","read":{"err":"b"}}]}"#.to_string(),
            format!(r#"{{"type":"give_back","text":"t","why":"w","pictures":[{{"reference":{},"bytes":"AA"}}]}}"#, reference(1).to_json()),
            r#"{"type":"folders","asked":["!"],"from":null,"read":{"err":"why"}}"#.to_string(),
            r#"{"type":"folders","asked":[],"from":7,"read":{"err":"why"}}"#.to_string(),
            r#"{"type":"folders","asked":[],"from":null,"read":{}}"#.to_string(),
            r#"{"type":"folders","asked":[],"from":null,"read":{"err":7}}"#.to_string(),
            r#"{"type":"folders","asked":[],"from":null,"read":{"err":"why","ok":{"folder":"Lw==","entries":[],"truncated":false}}}"#.to_string(),
            r#"{"type":"folders","asked":[],"from":null,"read":{"ok":{"entries":[],"truncated":false}}}"#.to_string(),
            r#"{"type":"folders","asked":[],"from":null,"read":{"ok":{"folder":"!","entries":[],"truncated":false}}}"#.to_string(),
            r#"{"type":"folders","asked":[],"from":null,"read":{"ok":{"folder":"Lw==","entries":[["only one"]],"truncated":false}}}"#.to_string(),
            r#"{"type":"folders","asked":[],"from":null,"read":{"ok":{"folder":"Lw==","entries":[["a",7]],"truncated":false}}}"#.to_string(),
            r#"{"type":"folders","asked":[],"from":null,"read":{"ok":{"folder":"Lw==","entries":[],"truncated":"no"}}}"#.to_string(),
        ] {
            assert!(decode_view(frame.as_bytes()).is_err(), "{frame}");
        }
    }

    /// A key reaches no error and no `Debug`: not in any frame that holds it
    /// and is refused for a fault of its own or another's.
    #[test]
    fn a_key_appears_in_no_refusal_and_no_debug_text() {
        let frames = [
            format!(r#"{{"type":"save_key","secret":"{SECRET}","replace":7}}"#),
            format!(r#"{{"type":"save_key","secret":"{SECRET}"}}"#),
            format!(r#"{{"type":"save_key","secret":"{SECRET} with a space","replace":true}}"#),
            format!("{{\"type\":\"save_key\",\"secret\":\"{SECRET}\\n\",\"replace\":true}}"),
            format!(r#"{{"type":"save_key","secret":["{SECRET}"],"replace":true}}"#),
            format!(r#"{{"type":"save_key","secret":"{SECRET}","replace":true"#),
            format!(r#"{{"type":"save_key","secret":"{SECRET}"#),
            format!(r#"{{"type":"{SECRET}"}}"#),
            format!(r#"{{"type":"save_key","{SECRET}":1,"{SECRET}":2}}"#),
            format!(r#"{{"type":"save_key","secret":{SECRET}}}"#),
            format!(
                r#"{{"type":"save_key","secret":"{}","replace":true}}"#,
                SECRET.repeat(20)
            ),
        ];
        for frame in &frames {
            let why = decode_request(frame.as_bytes()).unwrap_err();
            assert!(!why.contains(SECRET), "{frame}: {why}");
            assert!(!why.contains("sk-or"), "{frame}: {why}");
            let why = decode_view(frame.as_bytes()).unwrap_err();
            assert!(!why.contains(SECRET), "{frame}: {why}");
        }
        let request = Request::SaveKey {
            secret: Secret::new(SECRET.into()),
            replace: false,
        };
        let back = decode_request(&encode_request(&request)).unwrap();
        assert!(!format!("{request:?}").contains(SECRET));
        assert!(!format!("{back:?}").contains(SECRET));
    }

    /// What the frames of the conversations' socketpair and of their logs say
    /// does not change for being carried here.
    #[test]
    fn the_existing_forms_write_what_they_wrote() {
        let up = Up::Ask {
            call: 1,
            title: "t".into(),
            details: vec!["d".into()],
            always: Some(offers().remove(0)),
        };
        assert_eq!(
            String::from_utf8(up.encode()).unwrap(),
            r#"{"type":"ask","call":1,"title":"t","details":["d"],"always":{"allow":true,"rules":["shell cargo test"]}}"#
        );
        let up = Up::Ask {
            call: 1,
            title: "t".into(),
            details: Vec::new(),
            always: Some(Remembered::Crossing {
                op: Crossed::Read,
                to: id(1).to_string(),
            }),
        };
        assert_eq!(
            String::from_utf8(up.encode()).unwrap(),
            format!(
                r#"{{"type":"ask","call":1,"title":"t","details":[],"always":{{"crossing":"read","to":"{}"}}}}"#,
                id(1)
            )
        );
        assert_eq!(
            String::from_utf8(Up::Waking.encode()).unwrap(),
            r#"{"type":"waking"}"#
        );
        for up in ups() {
            assert_eq!(Up::decode(&up.encode()).unwrap(), up);
        }
        let saved = crate::config::templates_json(&templates()[..1]).to_string();
        assert_eq!(saved, r#"[{"name":"bare","repos":[]}]"#);
        let whole = Template {
            name: "whole".into(),
            repos: vec![
                crate::config::checked_repo(
                    "https://github.com/timmydo/td",
                    "main",
                    "b",
                    Some(vec!["td-agent".into()]),
                )
                .unwrap(),
                crate::config::checked_repo("https://github.com/a/b", "main", "c", None).unwrap(),
            ],
            shared: crate::config::shared_field("/a:rw /b").unwrap(),
            network: Some(Network::Off),
            system: true,
            mcp: Vec::new(),
        };
        assert_eq!(
            crate::config::templates_json(&[whole]).to_string(),
            r#"[{"name":"whole","network":"off","system":true,"shared":[{"path":"/a","write":true},{"path":"/b","write":false}],"repos":[{"remote":"https://github.com/timmydo/td","base":"main","branch":"b","sparse":["td-agent"]},{"remote":"https://github.com/a/b","base":"main","branch":"c","sparse":null}]}]"#
        );
    }
}
