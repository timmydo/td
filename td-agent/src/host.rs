//! What a conversation process and its tool host say to each other
//! (DESIGN.md §2, §12): one JSON object per `frame`, over the pipe the
//! conversation holds. It is multiplexed: each call carries an id the
//! conversation picks, its output comes back in pieces under that id
//! while it runs, and its end once, so calls run side by side and any one
//! can be cancelled. Frames are bounded both ways.
//!
//! **Every reply is jail-controlled data.** A process in the jail can
//! replace or impersonate the tool host, so what comes back (file text,
//! digests, output) is untrusted input, read within bounds and never
//! taken as authority. `Client` drops a reply for an id it did not ask
//! for.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::frame;
use crate::jail::Tail;
use td_json::Json;

/// The most calls one tool host runs at once; one more is refused.
pub const MAX_CALLS: usize = 16;
/// The most bytes of live output one `Output` frame carries.
pub const OUTPUT_CHUNK: usize = 32 * 1024;
/// The most replies read ahead of the conversation taking them.
const REPLY_QUEUE: usize = 64;
/// The most bytes of one `Link::Bytes`.
pub const LINK_CHUNK: usize = 32 * 1024;
/// The most bytes either end of a link sends ahead of the other end's
/// `Took` (DESIGN.md §10).
pub const LINK_WINDOW: u64 = 256 * 1024;
/// The most links one tool host holds open at once.
pub const MAX_LINKS: usize = 32;
/// The longest host a link's `Open` names, and the longest reason a
/// `Refused` gives.
const MAX_LINK_HOST: usize = 255;
pub const MAX_LINK_WHY: usize = 1024;
/// A binary frame's first byte, which no JSON text begins with, and its
/// one kind: a link's bytes.
const BINARY: u8 = 0;
const BINARY_BYTES: u8 = 1;

/// A tool call, as the tool host performs it (§12).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Call {
    Read {
        path: String,
        offset: Option<u64>,
        limit: Option<u64>,
    },
    /// `expected` is the digest of this conversation's last read or
    /// write of `path`, which a replacement must match.
    Write {
        path: String,
        content: String,
        expected: Option<String>,
    },
    Edit {
        path: String,
        old: String,
        new: String,
        all: bool,
        expected: Option<String>,
    },
    /// `apply_patch`: `patch` in Codex's grammar (`patch.rs`), with the
    /// digest of this conversation's last read or write of each existing
    /// file it changes, which that file must match.
    Patch {
        patch: String,
        expected: Vec<(String, String)>,
    },
    Glob {
        pattern: String,
        path: Option<String>,
    },
    Shell {
        command: String,
        timeout_ms: Option<u64>,
        workdir: Option<String>,
    },
    /// The review controller's command, with a private Cargo output path.
    ReviewShell {
        command: String,
        timeout_ms: Option<u64>,
        workdir: Option<String>,
        target_dir: String,
    },
    /// A `shell` call with `background`: its instance outlives the call
    /// that started it, answered when the command ends (DESIGN.md §12).
    Background {
        command: String,
        timeout_ms: Option<u64>,
        workdir: Option<String>,
    },
    Grep {
        pattern: String,
        path: Option<String>,
        include: Option<String>,
        exclude: Option<String>,
        extended: bool,
        ignore_case: bool,
        context: Option<u32>,
    },
    Sed {
        script: String,
        paths: Vec<String>,
        extended: bool,
    },
}

/// A proxied connection's frame (DESIGN.md §10), over the same pipe as
/// calls: the tool host's proxy opens a link and td-agent decides it,
/// then bytes go both ways, each end sending at most `LINK_WINDOW` past
/// what the other has said it `Took`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Link {
    /// Up: the proxy took a connection for `host:port`.
    Open { link: u64, host: String, port: u16 },
    /// Down: td-agent opened it through the egress relay.
    Opened { link: u64 },
    /// Down: td-agent did not, and why.
    Refused { link: u64, why: String },
    /// Either way: bytes, at most `LINK_CHUNK`.
    Bytes { link: u64, data: Vec<u8> },
    /// Either way: this end is done with the link.
    Shut { link: u64 },
    /// Either way: this end delivered `bytes` of the other's.
    Took { link: u64, bytes: u64 },
}

impl Link {
    pub fn id(&self) -> u64 {
        match self {
            Self::Open { link, .. }
            | Self::Opened { link }
            | Self::Refused { link, .. }
            | Self::Bytes { link, .. }
            | Self::Shut { link }
            | Self::Took { link, .. } => *link,
        }
    }

    /// A link's bytes as a binary frame, its other frames as JSON.
    fn encode(&self) -> Vec<u8> {
        let number = |n: u64| Json::Num(n.to_string());
        let json = match self {
            Self::Bytes { link, data } => {
                let mut bytes = Vec::with_capacity(data.len().saturating_add(10));
                bytes.push(BINARY);
                bytes.push(BINARY_BYTES);
                bytes.extend_from_slice(&link.to_be_bytes());
                bytes.extend_from_slice(data);
                return bytes;
            }
            Self::Open { link, host, port } => vec![
                ("link".into(), Json::Str("open".into())),
                ("id".into(), number(*link)),
                ("host".into(), Json::Str(host.clone())),
                ("port".into(), number(u64::from(*port))),
            ],
            Self::Opened { link } => vec![
                ("link".into(), Json::Str("opened".into())),
                ("id".into(), number(*link)),
            ],
            Self::Refused { link, why } => vec![
                ("link".into(), Json::Str("refused".into())),
                ("id".into(), number(*link)),
                ("why".into(), Json::Str(why.clone())),
            ],
            Self::Shut { link } => vec![
                ("link".into(), Json::Str("shut".into())),
                ("id".into(), number(*link)),
            ],
            Self::Took { link, bytes } => vec![
                ("link".into(), Json::Str("took".into())),
                ("id".into(), number(*link)),
                ("bytes".into(), number(*bytes)),
            ],
        };
        Json::Obj(json).to_string().into_bytes()
    }

    /// A binary frame: a link's bytes, within `LINK_CHUNK`.
    fn decode_binary(bytes: &[u8]) -> Result<Self, String> {
        let (Some(&BINARY_BYTES), Some(id), Some(data)) =
            (bytes.get(1), bytes.get(2..10), bytes.get(10..))
        else {
            return Err("a binary frame that is no link's bytes".into());
        };
        if data.is_empty() || data.len() > LINK_CHUNK {
            return Err(format!("a link's bytes are 1 to {LINK_CHUNK}"));
        }
        let mut link = [0u8; 8];
        link.copy_from_slice(id);
        Ok(Self::Bytes {
            link: u64::from_be_bytes(link),
            data: data.to_vec(),
        })
    }

    fn decode_json(value: &Json) -> Result<Self, String> {
        let link = id_of(value, "id")?;
        let kind = value.get("link").and_then(Json::as_str).unwrap_or_default();
        Ok(match kind {
            "open" => {
                let host = value
                    .get("host")
                    .and_then(Json::as_str)
                    .filter(|host| !host.is_empty() && host.len() <= MAX_LINK_HOST)
                    .ok_or("a link's open names no host")?;
                let port = value
                    .get("port")
                    .and_then(Json::as_u64)
                    .and_then(|port| u16::try_from(port).ok())
                    .filter(|port| *port != 0)
                    .ok_or("a link's open names no port")?;
                Self::Open {
                    link,
                    host: host.into(),
                    port,
                }
            }
            "opened" => Self::Opened { link },
            "refused" => Self::Refused {
                link,
                why: value
                    .get("why")
                    .and_then(Json::as_str)
                    .filter(|why| why.len() <= MAX_LINK_WHY)
                    .ok_or("a link's refusal says no why")?
                    .into(),
            },
            "shut" => Self::Shut { link },
            "took" => Self::Took {
                link,
                bytes: id_of(value, "bytes")?,
            },
            other => return Err(format!("no link frame {other:?}")),
        })
    }
}

/// How many bytes one direction of a link has sent that the other end
/// has not yet taken: a sender waits while `LINK_WINDOW` would be passed,
/// until the link closes.
pub struct Credit {
    state: Mutex<(u64, bool)>,
    wake: Condvar,
}

impl Default for Credit {
    fn default() -> Self {
        Self {
            state: Mutex::new((0, false)),
            wake: Condvar::new(),
        }
    }
}

impl Credit {
    /// Takes room for `n` bytes, waiting for it; false once closed.
    pub fn take(&self, n: u64) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        loop {
            if state.1 {
                return false;
            }
            if state.0.saturating_add(n) <= LINK_WINDOW {
                state.0 = state.0.saturating_add(n);
                return true;
            }
            state = match self.wake.wait(state) {
                Ok(state) => state,
                Err(_) => return false,
            };
        }
    }

    /// The other end took `n` bytes.
    pub fn give(&self, n: u64) {
        if let Ok(mut state) = self.state.lock() {
            state.0 = state.0.saturating_sub(n);
        }
        self.wake.notify_all();
    }

    /// No more is sent: a waiting sender is let go.
    pub fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.1 = true;
        }
        self.wake.notify_all();
    }
}

/// From a conversation to its tool host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Down {
    Call {
        id: u64,
        call: Call,
    },
    /// End call `id` early: its process is killed. A call already ended
    /// is nothing.
    Cancel {
        id: u64,
    },
    /// A link's: `Opened`, `Refused`, `Bytes`, `Shut` or `Took`.
    Link(Link),
}

/// What a call came to.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Done {
    /// What the model is shown.
    pub text: String,
    /// What the log keeps beyond that: a process's output, its head and
    /// tail (§12).
    pub kept: Option<String>,
    /// The file's digest after a read, write or edit, which the next
    /// replacement of it expects.
    pub digest: Option<String>,
    /// A patch's files: each one written with its digest now, and each
    /// one deleted or moved away with none, which is forgotten.
    pub digests: Vec<(String, Option<String>)>,
}

/// From a tool host to its conversation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Up {
    /// Output call `id`'s process wrote, as it came.
    Output { id: u64, text: String },
    /// Call `id` ended: what it came to, or why it was refused.
    Done {
        id: u64,
        outcome: Result<Done, String>,
    },
    /// A link's: `Open`, `Bytes`, `Shut` or `Took`.
    Link(Link),
}

fn opt_str(value: Option<&str>) -> Json {
    value.map_or(Json::Null, |v| Json::Str(v.into()))
}

fn opt_num(value: Option<u64>) -> Json {
    value.map_or(Json::Null, |v| Json::Num(v.to_string()))
}

/// A list of `[path, digest]` pairs, a digest null where none is, at
/// most two for each file a patch may name: untrusted, so bounded.
fn pairs(value: Option<&Json>, what: &str) -> Result<Vec<(String, Option<String>)>, String> {
    let Some(items) = value.and_then(Json::as_arr) else {
        return Err(format!("{what} is not a list"));
    };
    if items.len() > crate::patch::MAX_FILES.saturating_mul(2) {
        return Err(format!("{what} names too many files"));
    }
    items
        .iter()
        .map(|item| match item.as_arr() {
            Some([Json::Str(path), Json::Str(digest)]) => Ok((path.clone(), Some(digest.clone()))),
            Some([Json::Str(path), Json::Null]) => Ok((path.clone(), None)),
            _ => Err(format!("{what} holds something not a [path, digest] pair")),
        })
        .collect()
}

impl Call {
    /// The call's name on the wire: the tool's, as the model calls it,
    /// but for td-agent's own and a background `shell`.
    pub fn tool(&self) -> &'static str {
        match self {
            Self::Read { .. } => "read_file",
            Self::Write { .. } => "write_file",
            Self::Edit { .. } => "edit_file",
            Self::Patch { .. } => "apply_patch",
            Self::Glob { .. } => "glob",
            Self::Shell { .. } => "shell",
            Self::ReviewShell { .. } => "review_shell",
            Self::Background { .. } => "background",
            Self::Grep { .. } => "grep",
            Self::Sed { .. } => "sed",
        }
    }

    fn args(&self) -> Json {
        let member = |pairs: Vec<(&str, Json)>| {
            Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
        };
        match self {
            Self::Read {
                path,
                offset,
                limit,
            } => member(vec![
                ("path", Json::Str(path.clone())),
                ("offset", opt_num(*offset)),
                ("limit", opt_num(*limit)),
            ]),
            Self::Write {
                path,
                content,
                expected,
            } => member(vec![
                ("path", Json::Str(path.clone())),
                ("content", Json::Str(content.clone())),
                ("expected", opt_str(expected.as_deref())),
            ]),
            Self::Edit {
                path,
                old,
                new,
                all,
                expected,
            } => member(vec![
                ("path", Json::Str(path.clone())),
                ("old_string", Json::Str(old.clone())),
                ("new_string", Json::Str(new.clone())),
                ("replace_all", Json::Bool(*all)),
                ("expected", opt_str(expected.as_deref())),
            ]),
            Self::Patch { patch, expected } => member(vec![
                ("patch", Json::Str(patch.clone())),
                (
                    "expected",
                    Json::Arr(
                        expected
                            .iter()
                            .map(|(path, digest)| {
                                Json::Arr(vec![Json::Str(path.clone()), Json::Str(digest.clone())])
                            })
                            .collect(),
                    ),
                ),
            ]),
            Self::Glob { pattern, path } => member(vec![
                ("pattern", Json::Str(pattern.clone())),
                ("path", opt_str(path.as_deref())),
            ]),
            Self::ReviewShell {
                command,
                timeout_ms,
                workdir,
                target_dir,
            } => member(vec![
                ("command", Json::Str(command.clone())),
                ("timeout_ms", opt_num(*timeout_ms)),
                ("workdir", opt_str(workdir.as_deref())),
                ("target_dir", Json::Str(target_dir.clone())),
            ]),
            Self::Shell {
                command,
                timeout_ms,
                workdir,
            }
            | Self::Background {
                command,
                timeout_ms,
                workdir,
            } => member(vec![
                ("command", Json::Str(command.clone())),
                ("timeout_ms", opt_num(*timeout_ms)),
                ("workdir", opt_str(workdir.as_deref())),
            ]),
            Self::Grep {
                pattern,
                path,
                include,
                exclude,
                extended,
                ignore_case,
                context,
            } => member(vec![
                ("pattern", Json::Str(pattern.clone())),
                ("path", opt_str(path.as_deref())),
                ("include", opt_str(include.as_deref())),
                ("exclude", opt_str(exclude.as_deref())),
                ("extended", Json::Bool(*extended)),
                ("ignore_case", Json::Bool(*ignore_case)),
                ("context", opt_num(context.map(u64::from))),
            ]),
            Self::Sed {
                script,
                paths,
                extended,
            } => member(vec![
                ("script", Json::Str(script.clone())),
                (
                    "paths",
                    Json::Arr(paths.iter().map(|p| Json::Str(p.clone())).collect()),
                ),
                ("extended", Json::Bool(*extended)),
            ]),
        }
    }

    fn decode(tool: &str, args: &Json) -> Result<Self, String> {
        let text = |name: &str| -> Result<String, String> {
            args.get(name)
                .and_then(Json::as_str)
                .map(String::from)
                .ok_or_else(|| format!("{tool}: no `{name}`"))
        };
        let maybe = |name: &str| -> Result<Option<String>, String> {
            match args.get(name) {
                None | Some(Json::Null) => Ok(None),
                Some(v) => v
                    .as_str()
                    .map(|s| Some(s.to_string()))
                    .ok_or_else(|| format!("{tool}: `{name}` is not a string")),
            }
        };
        let number = |name: &str| -> Result<Option<u64>, String> {
            match args.get(name) {
                None | Some(Json::Null) => Ok(None),
                Some(v) => v
                    .as_u64()
                    .map(Some)
                    .ok_or_else(|| format!("{tool}: `{name}` is not a whole number")),
            }
        };
        let flag = |name: &str| args.get(name).is_some_and(Json::is_true);
        Ok(match tool {
            "review_shell" => Self::ReviewShell {
                command: text("command")?,
                timeout_ms: number("timeout_ms")?,
                workdir: maybe("workdir")?,
                target_dir: text("target_dir")?,
            },
            "read_file" => Self::Read {
                path: text("path")?,
                offset: number("offset")?,
                limit: number("limit")?,
            },
            "write_file" => Self::Write {
                path: text("path")?,
                content: text("content")?,
                expected: maybe("expected")?,
            },
            "edit_file" => Self::Edit {
                path: text("path")?,
                old: text("old_string")?,
                new: text("new_string")?,
                all: flag("replace_all"),
                expected: maybe("expected")?,
            },
            "apply_patch" => Self::Patch {
                patch: text("patch")?,
                expected: pairs(args.get("expected"), "apply_patch: `expected`")?
                    .into_iter()
                    .map(|(path, digest)| {
                        digest
                            .map(|digest| (path, digest))
                            .ok_or_else(|| "apply_patch: an `expected` digest is null".to_string())
                    })
                    .collect::<Result<_, _>>()?,
            },
            "glob" => Self::Glob {
                pattern: text("pattern")?,
                path: maybe("path")?,
            },
            "shell" => Self::Shell {
                command: text("command")?,
                timeout_ms: number("timeout_ms")?,
                workdir: maybe("workdir")?,
            },
            "background" => Self::Background {
                command: text("command")?,
                timeout_ms: number("timeout_ms")?,
                workdir: maybe("workdir")?,
            },
            "grep" => Self::Grep {
                pattern: text("pattern")?,
                path: maybe("path")?,
                include: maybe("include")?,
                exclude: maybe("exclude")?,
                extended: flag("extended"),
                ignore_case: flag("ignore_case"),
                context: number("context")?.map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
            },
            "sed" => Self::Sed {
                script: text("script")?,
                paths: args
                    .get("paths")
                    .and_then(Json::as_arr)
                    .ok_or_else(|| format!("{tool}: no `paths`"))?
                    .iter()
                    .map(|p| {
                        p.as_str()
                            .map(String::from)
                            .ok_or_else(|| format!("{tool}: a path is not a string"))
                    })
                    .collect::<Result<_, _>>()?,
                extended: flag("extended"),
            },
            other => return Err(format!("no tool {other:?}")),
        })
    }
}

fn id_of(value: &Json, name: &str) -> Result<u64, String> {
    value
        .get(name)
        .and_then(Json::as_u64)
        .ok_or_else(|| format!("no `{name}` id"))
}

impl Down {
    pub fn encode(&self) -> Vec<u8> {
        let json = match self {
            Self::Call { id, call } => Json::Obj(vec![
                ("call".into(), Json::Num(id.to_string())),
                ("tool".into(), Json::Str(call.tool().into())),
                ("args".into(), call.args()),
            ]),
            Self::Cancel { id } => Json::Obj(vec![("cancel".into(), Json::Num(id.to_string()))]),
            Self::Link(link) => return link.encode(),
        };
        json.to_string().into_bytes()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.first() == Some(&BINARY) {
            return Link::decode_binary(bytes).map(Self::Link);
        }
        let value = td_json::parse_slice(bytes).map_err(|e| e.to_string())?;
        if value.get("link").is_some() {
            return match Link::decode_json(&value)? {
                Link::Open { .. } => Err("an open goes up, not down".into()),
                link => Ok(Self::Link(link)),
            };
        }
        if value.get("cancel").is_some() {
            return Ok(Self::Cancel {
                id: id_of(&value, "cancel")?,
            });
        }
        let id = id_of(&value, "call")?;
        let tool = value
            .get("tool")
            .and_then(Json::as_str)
            .ok_or("a call names no tool")?;
        let args = value.get("args").ok_or("a call has no arguments")?;
        Ok(Self::Call {
            id,
            call: Call::decode(tool, args)?,
        })
    }
}

impl Up {
    pub fn encode(&self) -> Vec<u8> {
        let json = match self {
            Self::Output { id, text } => Json::Obj(vec![
                ("output".into(), Json::Num(id.to_string())),
                ("text".into(), Json::Str(text.clone())),
            ]),
            Self::Done { id, outcome } => {
                let mut pairs = vec![("done".into(), Json::Num(id.to_string()))];
                match outcome {
                    Ok(done) => {
                        pairs.push(("text".into(), Json::Str(done.text.clone())));
                        pairs.push(("kept".into(), opt_str(done.kept.as_deref())));
                        pairs.push(("digest".into(), opt_str(done.digest.as_deref())));
                        if !done.digests.is_empty() {
                            pairs.push((
                                "digests".into(),
                                Json::Arr(
                                    done.digests
                                        .iter()
                                        .map(|(path, digest)| {
                                            Json::Arr(vec![
                                                Json::Str(path.clone()),
                                                opt_str(digest.as_deref()),
                                            ])
                                        })
                                        .collect(),
                                ),
                            ));
                        }
                    }
                    Err(why) => pairs.push(("error".into(), Json::Str(why.clone()))),
                }
                Json::Obj(pairs)
            }
            Self::Link(link) => return link.encode(),
        };
        json.to_string().into_bytes()
    }

    /// A reply, read as the untrusted data it is: well formed or refused.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.first() == Some(&BINARY) {
            return Link::decode_binary(bytes).map(Self::Link);
        }
        let value = td_json::parse_slice(bytes).map_err(|e| e.to_string())?;
        if value.get("link").is_some() {
            return match Link::decode_json(&value)? {
                Link::Opened { .. } | Link::Refused { .. } => {
                    Err("an opening's answer goes down, not up".into())
                }
                link => Ok(Self::Link(link)),
            };
        }
        let text = |name: &str| value.get(name).and_then(Json::as_str).map(String::from);
        if value.get("output").is_some() {
            return Ok(Self::Output {
                id: id_of(&value, "output")?,
                text: text("text").ok_or("output with no text")?,
            });
        }
        let id = id_of(&value, "done")?;
        let outcome = match text("error") {
            Some(why) => Err(why),
            None => Ok(Done {
                text: text("text").ok_or("a reply with no text")?,
                kept: text("kept"),
                digest: text("digest"),
                digests: match value.get("digests") {
                    None => Vec::new(),
                    digests => pairs(digests, "a reply's `digests`")?,
                },
            }),
        };
        Ok(Self::Done { id, outcome })
    }
}

/// A conversation's end of its tool host: calls go down, replies come up
/// on a thread that reads them, and only replies to calls in flight are
/// passed on.
pub struct Client {
    writer: Arc<crate::egress::Pipe>,
    replies: Receiver<Result<Up, String>>,
    next: u64,
    open: BTreeSet<u64>,
    child: Option<Child>,
    /// A jailed host's spec, removed with it, and the tail of td-jail's
    /// own diagnostics (crate::jail).
    spec: Option<PathBuf>,
    diagnostic: Option<Arc<Tail>>,
    /// A failure has waited for td-jail's account once; later ones do not.
    waited: bool,
}

/// How long a failure waits for td-jail to finish saying why.
const FAILURE_WAIT: Duration = Duration::from_secs(2);
/// How long dropping a jailed host waits for its instance to let go of
/// the channel.
const DROP_WAIT: Duration = Duration::from_secs(5);

impl Client {
    /// Speaks to a tool host over `reader` and `writer`, with no links:
    /// a link the host opens is refused.
    pub fn over(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static) -> Self {
        Self::linked(reader, writer, None)
    }

    /// Speaks to a tool host over `reader` and `writer`, its links judged
    /// and opened as `linked` says (DESIGN.md §10), or refused without one.
    pub fn linked(
        mut reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        linked: Option<crate::egress::Linked>,
    ) -> Self {
        let writer = crate::egress::Pipe::new(writer, crate::egress::PIPE_WRITE);
        let links = crate::egress::Links::new(linked, Arc::clone(&writer));
        // Bounded, so a tool host that floods is held at the pipe, not in
        // this process's memory.
        let (send, replies) = mpsc::sync_channel(REPLY_QUEUE);
        std::thread::spawn(move || {
            loop {
                let reply = match frame::read(&mut reader) {
                    Ok(None) => break,
                    Ok(Some(bytes)) => Up::decode(&bytes),
                    Err(e) => {
                        let _ = send.send(Err(format!("the tool host: {e}")));
                        break;
                    }
                };
                // A link's frame is never a call's reply, and never waits
                // for the conversation to take one.
                let reply = match reply {
                    Ok(Up::Link(link)) => {
                        links.up(link);
                        continue;
                    }
                    other => other,
                };
                if send.send(reply).is_err() {
                    break;
                }
            }
            links.close_all();
        });
        Self {
            writer,
            replies,
            next: 1,
            open: BTreeSet::new(),
            child: None,
            spec: None,
            diagnostic: None,
            waited: false,
        }
    }

    /// This client with the jail instance it speaks to, killed when the
    /// client is dropped, and that instance's spec and diagnostics.
    pub(crate) fn owning(mut self, child: Child, spec: PathBuf, diagnostic: Arc<Tail>) -> Self {
        self.child = Some(child);
        self.spec = Some(spec);
        self.diagnostic = Some(diagnostic);
        self
    }

    /// What td-jail has said so far, when this host is jailed and it said
    /// anything.
    pub fn diagnostic(&self) -> Option<String> {
        self.diagnostic.as_ref()?.text(Duration::ZERO)
    }

    /// `what` went wrong with the host; for a jailed one, with what td-jail
    /// said and how it ended, the first failure giving it a moment to
    /// finish both.
    fn failed(&mut self, what: String) -> String {
        let Some(tail) = &self.diagnostic else {
            return what;
        };
        let wait = if self.waited {
            Duration::ZERO
        } else {
            FAILURE_WAIT
        };
        self.waited = true;
        let mut why = what;
        if let Some(said) = tail.text(wait) {
            why.push_str(": ");
            why.push_str(&said);
        }
        let ended = self
            .child
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten());
        if let Some(status) = ended {
            why.push_str(&format!(" (td-jail {status})"));
        }
        why
    }

    /// Starts `program tool-host` over its standard input and output, for
    /// `roots` with td-txt at `txt`. The jail launch replaces this (§8).
    pub fn spawn(program: &Path, roots: &[PathBuf], txt: Option<&Path>) -> Result<Self, String> {
        let mut command = Command::new(program);
        command.arg("tool-host");
        if let Some(txt) = txt {
            command.arg("--txt").arg(txt);
        }
        for root in roots {
            command.arg("--root").arg(root);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("the tool host: {e}"))?;
        let stdin: ChildStdin = child.stdin.take().ok_or("the tool host has no input")?;
        let stdout = child.stdout.take().ok_or("the tool host has no output")?;
        let mut client = Self::over(stdout, stdin);
        client.child = Some(child);
        Ok(client)
    }

    /// Sends `call`, returning its id.
    pub fn call(&mut self, call: Call) -> Result<u64, String> {
        let id = self.next;
        self.next = self.next.saturating_add(1);
        let sent = self.write(&Down::Call { id, call }.encode());
        sent.map_err(|e| self.failed(format!("the tool host: {e}")))?;
        self.open.insert(id);
        Ok(id)
    }

    /// Asks for call `id` to end early.
    pub fn cancel(&mut self, id: u64) -> Result<(), String> {
        let sent = self.write(&Down::Cancel { id }.encode());
        sent.map_err(|e| self.failed(format!("the tool host: {e}")))
    }

    /// Writes one frame down, the links' frames waiting their turn.
    fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.frame(bytes)
    }

    /// The next reply to a call in flight, waiting at most `wait`; none when
    /// none came. An error means the tool host is gone or spoke nonsense,
    /// and every call in flight is lost.
    pub fn next_reply(&mut self, wait: Duration) -> Option<Result<Up, String>> {
        // One deadline, so replies that are dropped do not stretch the wait.
        let deadline = Instant::now().checked_add(wait);
        loop {
            let left = deadline.map_or(wait, |at| at.saturating_duration_since(Instant::now()));
            let reply = match self.replies.recv_timeout(left) {
                Ok(reply) => reply,
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) if self.open.is_empty() => return None,
                Err(RecvTimeoutError::Disconnected) => {
                    let why = "the tool host ended with calls in flight".into();
                    return Some(Err(self.failed(why)));
                }
            };
            match reply {
                Ok(Up::Output { id, .. }) if !self.open.contains(&id) => continue,
                Ok(Up::Done { id, .. }) if !self.open.remove(&id) => continue,
                Err(e) => return Some(Err(self.failed(e))),
                other => return Some(other),
            }
        }
    }

    /// The calls sent and not ended.
    pub fn in_flight(&self) -> usize {
        self.open.len()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(spec) = &self.spec {
            let _ = std::fs::remove_file(spec);
            // Killing td-jail's outer process takes the instance down
            // through each stage's parent-death signal. Its stage 2 and
            // tool host hold the channel's other end until they are gone,
            // so the reader's end of it is the instance's.
            let deadline = Instant::now().checked_add(DROP_WAIT);
            while let Some(left) = deadline.map(|at| at.saturating_duration_since(Instant::now())) {
                if left.is_zero() || self.replies.recv_timeout(left).is_err() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Every link frame crosses whole, each in its direction, a link's
    /// bytes as a binary frame; the wrong direction's, bytes past
    /// `LINK_CHUNK` or none, and an open with no host or port are refused.
    #[test]
    fn link_frames_round_trip_each_its_way() {
        let ups = [
            Link::Open {
                link: 7,
                host: "static.crates.io".into(),
                port: 443,
            },
            Link::Bytes {
                link: u64::MAX,
                data: vec![0, 1, 2, b'{'],
            },
            Link::Shut { link: 3 },
            Link::Took {
                link: 3,
                bytes: 1 << 40,
            },
        ];
        for link in ups {
            let bytes = Up::Link(link.clone()).encode();
            assert_eq!(Up::decode(&bytes), Ok(Up::Link(link.clone())), "{link:?}");
        }
        let downs = [
            Link::Opened { link: 7 },
            Link::Refused {
                link: 7,
                why: "not on the allowlist".into(),
            },
            Link::Bytes {
                link: 9,
                data: vec![b'x'; LINK_CHUNK],
            },
            Link::Shut { link: 9 },
            Link::Took { link: 9, bytes: 5 },
        ];
        for link in downs {
            let bytes = Down::Link(link.clone()).encode();
            assert_eq!(
                Down::decode(&bytes),
                Ok(Down::Link(link.clone())),
                "{link:?}"
            );
        }
        assert_eq!(
            Up::Link(Link::Bytes {
                link: 1,
                data: b"a".to_vec()
            })
            .encode(),
            [0, 1, 0, 0, 0, 0, 0, 0, 0, 1, b'a']
        );
        let opened = Down::Link(Link::Opened { link: 1 }).encode();
        assert!(Up::decode(&opened).is_err());
        let open = Up::Link(Link::Open {
            link: 1,
            host: "h".into(),
            port: 1,
        })
        .encode();
        assert!(Down::decode(&open).is_err());
        for bad in [
            vec![0, 1, 0, 0, 0, 0, 0, 0, 0, 1],
            vec![0, 2, 0, 0, 0, 0, 0, 0, 0, 1, b'a'],
            vec![0, 1, 0, 0],
            [
                vec![0, 1, 0, 0, 0, 0, 0, 0, 0, 1],
                vec![b'x'; LINK_CHUNK + 1],
            ]
            .concat(),
            br#"{"link":"open","id":1,"host":"","port":443}"#.to_vec(),
            br#"{"link":"open","id":1,"host":"h","port":0}"#.to_vec(),
            br#"{"link":"open","id":1,"host":"h","port":65536}"#.to_vec(),
            br#"{"link":"open","id":1,"port":443}"#.to_vec(),
            br#"{"link":"wave","id":1}"#.to_vec(),
            br#"{"link":"shut"}"#.to_vec(),
        ] {
            assert!(Up::decode(&bad).is_err(), "{bad:?}");
        }
        let long = format!(
            r#"{{"link":"open","id":1,"host":"{}","port":443}}"#,
            "h".repeat(MAX_LINK_HOST + 1)
        );
        assert!(Up::decode(long.as_bytes()).is_err());
    }

    /// A sender waits while the window would be passed, goes on when the
    /// other end takes, and is let go when the link closes.
    #[test]
    fn credit_holds_a_sender_to_the_window() {
        let credit = Arc::new(Credit::default());
        assert!(credit.take(LINK_WINDOW));
        let (tell, told) = mpsc::channel();
        let waiting = Arc::clone(&credit);
        std::thread::spawn(move || {
            let _ = tell.send(waiting.take(1));
        });
        assert!(told.recv_timeout(Duration::from_millis(200)).is_err());
        credit.give(1);
        assert_eq!(told.recv_timeout(Duration::from_secs(5)), Ok(true));
        let (tell, told) = mpsc::channel();
        let waiting = Arc::clone(&credit);
        std::thread::spawn(move || {
            let _ = tell.send(waiting.take(1));
        });
        assert!(told.recv_timeout(Duration::from_millis(200)).is_err());
        credit.close();
        assert_eq!(told.recv_timeout(Duration::from_secs(5)), Ok(false));
        assert!(!credit.take(0));
    }

    #[test]
    fn a_patch_and_its_digests_round_trip() {
        let call = Call::Patch {
            patch: "*** Begin Patch\n*** Delete File: /w/a\n*** End Patch\n".into(),
            expected: vec![("/w/a".into(), "d1".into())],
        };
        assert_eq!(Call::decode(call.tool(), &call.args()).unwrap(), call);
        let up = Up::Done {
            id: 7,
            outcome: Ok(Done {
                text: "applied".into(),
                digests: vec![("/w/a".into(), None), ("/w/b".into(), Some("d2".into()))],
                ..Done::default()
            }),
        };
        assert_eq!(Up::decode(&up.encode()).unwrap(), up);
        // A reply from before patches has none; a malformed list is refused.
        let old = br#"{"done":7,"text":"x","kept":null,"digest":null}"#;
        assert!(matches!(
            Up::decode(old).unwrap(),
            Up::Done { outcome: Ok(Done { digests, .. }), .. } if digests.is_empty()
        ));
        let bad = br#"{"done":7,"text":"x","digests":[["/w/a",3]]}"#;
        assert!(Up::decode(bad).is_err());
    }

    #[test]
    fn calls_and_replies_round_trip() {
        let calls = [
            Call::Read {
                path: "/w/a".into(),
                offset: Some(3),
                limit: None,
            },
            Call::Write {
                path: "/w/a".into(),
                content: "x\n\"y\"".into(),
                expected: Some("ab".into()),
            },
            Call::Edit {
                path: "/w/a".into(),
                old: "a".into(),
                new: "b".into(),
                all: true,
                expected: None,
            },
            Call::Glob {
                pattern: "**/*.rs".into(),
                path: None,
            },
            Call::Shell {
                command: "ls".into(),
                timeout_ms: Some(5),
                workdir: Some("/w".into()),
            },
            Call::Background {
                command: "make".into(),
                timeout_ms: None,
                workdir: Some("/w".into()),
            },
            Call::Grep {
                pattern: "x".into(),
                path: None,
                include: Some("*.rs".into()),
                exclude: None,
                extended: true,
                ignore_case: false,
                context: Some(2),
            },
            Call::Sed {
                script: "s/a/b/".into(),
                paths: vec!["/w/a".into()],
                extended: false,
            },
        ];
        for (n, call) in calls.into_iter().enumerate() {
            let down = Down::Call { id: n as u64, call };
            assert_eq!(Down::decode(&down.encode()).unwrap(), down);
        }
        let cancel = Down::Cancel { id: 9 };
        assert_eq!(Down::decode(&cancel.encode()).unwrap(), cancel);
        for up in [
            Up::Output {
                id: 1,
                text: "out\n".into(),
            },
            Up::Done {
                id: 1,
                outcome: Ok(Done {
                    text: "t".into(),
                    kept: Some("k".into()),
                    digest: None,
                    digests: Vec::new(),
                }),
            },
            Up::Done {
                id: 2,
                outcome: Err("no".into()),
            },
        ] {
            assert_eq!(Up::decode(&up.encode()).unwrap(), up);
        }
        assert!(Up::decode(b"{\"done\":\"x\"}").is_err());
        assert!(Up::decode(b"{\"done\":1}").is_err());
        assert!(Down::decode(br#"{"call":1,"tool":"rm","args":{}}"#)
            .unwrap_err()
            .contains("no tool"));
    }

    /// A reply for a call the client did not make, or made and saw end, is
    /// dropped: the tool host is not taken at its word.
    #[test]
    fn replies_to_no_call_in_flight_are_dropped() {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut client = Client::over(ours.try_clone().unwrap(), ours);
        let id = client
            .call(Call::Glob {
                pattern: "*".into(),
                path: None,
            })
            .unwrap();
        let mut host = theirs;
        let done = |id| Up::Done {
            id,
            outcome: Err("x".into()),
        };
        for up in [
            Up::Output {
                id: 77,
                text: "forged".into(),
            },
            done(77),
            done(id),
            done(id),
        ] {
            frame::write(&mut host, &up.encode()).unwrap();
        }
        let first = client.next_reply(Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(first, done(id));
        assert!(client.next_reply(Duration::from_millis(100)).is_none());
        assert_eq!(client.in_flight(), 0);
        let _ = Down::decode(&frame::read(&mut host).unwrap().unwrap()).unwrap();
    }
}
