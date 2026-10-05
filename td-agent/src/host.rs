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
use std::sync::Arc;
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
    Glob {
        pattern: String,
        path: Option<String>,
    },
    Shell {
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
    /// td-agent's own, never a model's: each of `checkouts` recorded as a
    /// git tree onto its snapshot ref with `git`, the host's by the path
    /// it resolves to, and, given `before`'s trees in their order, the
    /// files changed since named (DESIGN.md §12).
    Snapshot {
        git: String,
        checkouts: Vec<String>,
        before: Vec<String>,
    },
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
}

fn opt_str(value: Option<&str>) -> Json {
    value.map_or(Json::Null, |v| Json::Str(v.into()))
}

fn opt_num(value: Option<u64>) -> Json {
    value.map_or(Json::Null, |v| Json::Num(v.to_string()))
}

impl Call {
    /// The tool's name, as the model calls it.
    pub fn tool(&self) -> &'static str {
        match self {
            Self::Read { .. } => "read_file",
            Self::Write { .. } => "write_file",
            Self::Edit { .. } => "edit_file",
            Self::Glob { .. } => "glob",
            Self::Shell { .. } => "shell",
            Self::Grep { .. } => "grep",
            Self::Sed { .. } => "sed",
            Self::Snapshot { .. } => "snapshot",
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
            Self::Glob { pattern, path } => member(vec![
                ("pattern", Json::Str(pattern.clone())),
                ("path", opt_str(path.as_deref())),
            ]),
            Self::Shell {
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
            Self::Snapshot {
                git,
                checkouts,
                before,
            } => member(vec![
                ("git", Json::Str(git.clone())),
                (
                    "checkouts",
                    Json::Arr(checkouts.iter().cloned().map(Json::Str).collect()),
                ),
                (
                    "before",
                    Json::Arr(before.iter().cloned().map(Json::Str).collect()),
                ),
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
        let texts = |name: &str| -> Result<Vec<String>, String> {
            args.get(name)
                .and_then(Json::as_arr)
                .ok_or_else(|| format!("{tool}: no `{name}`"))?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(String::from)
                        .ok_or_else(|| format!("{tool}: `{name}` holds something not a string"))
                })
                .collect()
        };
        Ok(match tool {
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
            "glob" => Self::Glob {
                pattern: text("pattern")?,
                path: maybe("path")?,
            },
            "shell" => Self::Shell {
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
            "snapshot" => Self::Snapshot {
                git: text("git")?,
                checkouts: texts("checkouts")?,
                before: texts("before")?,
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
        };
        json.to_string().into_bytes()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let value = td_json::parse_slice(bytes).map_err(|e| e.to_string())?;
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
                    }
                    Err(why) => pairs.push(("error".into(), Json::Str(why.clone()))),
                }
                Json::Obj(pairs)
            }
        };
        json.to_string().into_bytes()
    }

    /// A reply, read as the untrusted data it is: well formed or refused.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let value = td_json::parse_slice(bytes).map_err(|e| e.to_string())?;
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
            }),
        };
        Ok(Self::Done { id, outcome })
    }
}

/// A conversation's end of its tool host: calls go down, replies come up
/// on a thread that reads them, and only replies to calls in flight are
/// passed on.
pub struct Client {
    writer: Box<dyn Write + Send>,
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
    /// Speaks to a tool host over `reader` and `writer`.
    pub fn over(
        mut reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
    ) -> Self {
        // Bounded, so a tool host that floods is held at the pipe, not in
        // this process's memory.
        let (send, replies) = mpsc::sync_channel(REPLY_QUEUE);
        std::thread::spawn(move || loop {
            let reply = match frame::read(&mut reader) {
                Ok(None) => return,
                Ok(Some(bytes)) => Up::decode(&bytes),
                Err(e) => {
                    let _ = send.send(Err(format!("the tool host: {e}")));
                    return;
                }
            };
            if send.send(reply).is_err() {
                return;
            }
        });
        Self {
            writer: Box::new(writer),
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
        let sent = frame::write(&mut self.writer, &Down::Call { id, call }.encode());
        sent.map_err(|e| self.failed(format!("the tool host: {e}")))?;
        self.open.insert(id);
        Ok(id)
    }

    /// Asks for call `id` to end early.
    pub fn cancel(&mut self, id: u64) -> Result<(), String> {
        let sent = frame::write(&mut self.writer, &Down::Cancel { id }.encode());
        sent.map_err(|e| self.failed(format!("the tool host: {e}")))
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
            Call::Snapshot {
                git: "/usr/bin/git".into(),
                checkouts: vec!["/w/a".into(), "/w/b".into()],
                before: vec!["a".repeat(40), "b".repeat(40)],
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
