//! `/review` (DESIGN.md §15, `/review`): the person's review of a
//! revision of the workspace's first worktree. The commit is staged as
//! a push's is: exported as a pack by a maintenance instance and
//! imported into a repository of the conversation's own that no jail can
//! write, borrowing the store's objects. `td-agent review` then reviews
//! that repository beside the conversation process; it is reserved and
//! charged as one request, and its text is logged for the model to read
//! as a review, not as the person's words.

use std::ffi::OsString;
use std::io::Read;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use td_json::Json;

use super::{prepared_entry, Session};
use crate::cost;
use crate::store::{Basis, Effect, Kind, Purpose, StateDir};

/// The most of a review's text kept: six times it, td-json's worst case
/// for control characters, fits one log line (`store::MAX_LINE`).
const MAX_REVIEW: u64 = 128 * 1024;

/// The most of the review's standard error read for its session log's
/// line, and the most of its end a failure quotes.
const MAX_STDERR_HEAD: usize = 64 * 1024;
const STDERR_TAIL: usize = 2048;

/// How often the wait for the review hears the window.
const POLL: Duration = Duration::from_millis(200);

/// The line `td-agent review` names its session log on, before it asks
/// anything of a model.
const SESSION_LOG: &str = "td-agent review: session log ";

/// How a review's process ended.
enum Ended {
    Exited(std::process::ExitStatus),
    /// Killed for an interrupt, or the window going.
    Interrupted,
    /// Killed when it could not be waited on, and why.
    Unwaited(String),
}

/// What a review's run came to.
struct Ran {
    ended: Ended,
    review: Vec<u8>,
    cut: bool,
    head: String,
    tail: String,
}

/// A commit staged for a review: the repository it is in.
struct Staged {
    repository: PathBuf,
    commit: String,
}

impl Session {
    /// The person's `/review` of `revision`, an effect of its own as a
    /// compaction is: started, run, and finished with what it came to,
    /// which the window shows.
    pub(super) fn review(&mut self, revision: String) -> Result<(), String> {
        let of = self.conversation.events().last().map_or(0, |e| e.seq);
        let started = self
            .log(Kind::Started {
                effect: Effect::Review,
                of,
            })?
            .seq;
        self.sync()?;
        self.interrupt = false;
        let stage = StateDir::at(self.state.clone()).review_stage(&self.conversation.meta().id);
        let outcome = self.reviewing(started, &revision, &stage);
        // Whatever it came to, nothing of the staging is kept.
        let _ = std::fs::remove_dir_all(&stage);
        self.log(Kind::Finished {
            started,
            outcome: outcome?,
            retry: false,
        })?;
        self.sync()
    }

    /// The review's work, staged in `stage`: its outcome.
    fn reviewing(&mut self, started: u64, revision: &str, stage: &Path) -> Result<String, String> {
        let could_not = |why: &str| format!("the review of {revision} did not run: {why}");
        // The window checked it; this process takes nothing on trust.
        if crate::commands::review(&format!("/review {revision}"))
            .is_none_or(|parsed| parsed.as_deref() != Ok(revision))
        {
            return Ok(could_not("that is not a revision /review takes"));
        }
        let asking = match self.asking()? {
            Ok(asking) => asking,
            Err(why) => return Ok(could_not(&why)),
        };
        let client = &asking.client;
        // The conversation's effort, for its own model where it takes one.
        let (model, effort) = match &client.review_model {
            Some(model) if *model != asking.name => (model.clone(), None),
            _ => {
                let takes = asking
                    .model
                    .as_ref()
                    .is_none_or(|m| m.supports("reasoning"));
                (
                    asking.name.clone(),
                    takes.then(|| asking.reasoning_effort.clone()),
                )
            }
        };
        let cap = client.review_max_cost;
        if let Err(why) = cost::within(
            "max_cost_per_conversation",
            client.limits.conversation,
            crate::accounts::spent(self.conversation.events()),
            cap,
        ) {
            return Ok(could_not(&why));
        }
        // The review event's text, the notices and the request's records,
        // at the store's worst case of six bytes a byte.
        let room = MAX_REVIEW
            .saturating_add(STDERR_TAIL as u64 * 2)
            .saturating_add(16 * 1024)
            .saturating_mul(6);
        if !self.conversation.has_room_for(room) {
            return Ok(could_not("the conversation's log is full"));
        }
        // Staged only once the cheap refusals pass; the export is heard
        // from only when it ends, as a push's is.
        let staged = match self.stage(revision, stage) {
            Ok(staged) => staged,
            Err(why) => return Ok(could_not(&why)),
        };
        self.hear();
        if self.interrupt {
            return Ok(could_not("it was interrupted before it began"));
        }
        let id = match self.reserve(cap) {
            Ok(id) => id,
            Err(why) => return Ok(could_not(&why)),
        };
        if self.interrupt {
            self.spent(id, 0);
            return Ok(could_not("it was interrupted before it began"));
        }
        let arguments = arguments(
            &staged.repository,
            &staged.commit,
            &model,
            effort.as_deref(),
            asking.routing.as_deref(),
            cap,
        );
        let request = self
            .log(Kind::Request {
                turn: started,
                purpose: Purpose::Review,
                prefix: 0,
                head: head(revision, &staged.commit, &model, cap),
                bytes: 0,
                reserved: cap,
            })?
            .seq;
        self.sync()?;
        let ran = match self.run_review(&arguments) {
            Ok(ran) => ran,
            Err(why) => {
                self.settle(request, None, (0, Basis::Nothing), why.clone())?;
                self.spent(id, 0);
                return Ok(could_not(&why));
            }
        };
        let (charged, basis) = charge(&ran.head, cap);
        let log = session_log(&ran.head)
            .map(|path| format!("; its session log is {}", path.display()))
            .unwrap_or_default();
        let review = String::from_utf8_lossy(&ran.review).trim().to_string();
        let spent = cost::show(charged);
        let (outcome, reviewed) = match ran.ended {
            Ended::Unwaited(why) => (
                format!(
                    "the review of {revision} could not be waited on ({why}) and was stopped, having spent {spent}{log}"
                ),
                false,
            ),
            Ended::Interrupted => (
                format!("the review of {revision} was interrupted, having spent {spent}{log}"),
                false,
            ),
            Ended::Exited(status) if status.success() && !review.is_empty() => (
                format!("reviewed {revision} with {model} for {spent}{log}"),
                true,
            ),
            Ended::Exited(status) if status.success() => (
                format!("the review of {revision} wrote no review, having spent {spent}{log}"),
                false,
            ),
            Ended::Exited(status) => {
                let tail = ran.tail.trim();
                (
                    format!(
                        "the review of {revision} failed ({status}), having spent {spent}{log}{}",
                        if tail.is_empty() {
                            String::new()
                        } else {
                            format!(":\n{tail}")
                        }
                    ),
                    false,
                )
            }
        };
        self.settle(request, None, (charged, basis), outcome.clone())?;
        self.spent(id, charged);
        if reviewed {
            let mut text = review;
            if ran.cut {
                text.push_str(&format!(
                    "\n\n[td-agent: the review was longer than {} KiB, and only its start is kept.]",
                    MAX_REVIEW / 1024
                ));
            }
            self.log(Kind::Review {
                request,
                revision: revision.to_string(),
                model,
                text,
            })?;
        }
        Ok(outcome)
    }

    /// Stages `revision` of the workspace's first worktree in `stage`, a
    /// push's way (DESIGN.md §9, Pushing): a maintenance instance exports
    /// the objects the store lacks as a pack, and they are imported
    /// strictly into a repository no jail can write, which borrows the
    /// store's objects. No git outside a jail reads the workspace's.
    fn stage(&mut self, revision: &str, stage: &Path) -> Result<Staged, String> {
        let meta = self.conversation.meta().clone();
        let Some(crate::workspace::Workspace::Repositories(repositories)) = &meta.workspace else {
            return Err("/review reviews a repository workspace's worktree, and this conversation's workspace is not one".into());
        };
        let first = repositories
            .entries
            .first()
            .ok_or("this workspace has no worktree")?;
        let worktree = first.checkout.to_string_lossy().into_owned();
        let (_, entry) = prepared_entry(&meta, "/review", &worktree)?;
        let base = meta
            .tracked
            .iter()
            .find(|tracked| tracked.remote == entry.remote && tracked.base == entry.base)
            .map(|tracked| tracked.id.clone())
            .ok_or_else(|| {
                format!(
                    "where {}'s {} is upstream is not known yet",
                    entry.remote, entry.base
                )
            })?;
        let _ = std::fs::remove_dir_all(stage);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(stage)
            .map_err(|e| format!("{}: {e}", stage.display()))?;
        let pack = stage.join("review.pack");
        let commit =
            super::export_revision(&self.state, &meta.id, entry, revision, &base, &pack)
                .map_err(|why| format!("{revision} of {worktree} could not be exported: {why}"))?;
        let worker = crate::git::Worker::local(&stage.join("git"), &crate::git::kept_env())?;
        let repository = stage.join("review.git");
        worker.publish(&repository, &entry.store)?;
        worker.import(&repository, &pack, &commit)?;
        let _ = std::fs::remove_file(&pack);
        Ok(Staged { repository, commit })
    }

    /// Runs `td-agent review` with `arguments`, hearing the window as it
    /// does: an interrupt, or the window going, kills it.
    fn run_review(&mut self, arguments: &[OsString]) -> Result<Ran, String> {
        let program = std::env::current_exe().map_err(|e| format!("this program's path: {e}"))?;
        let mut child = Command::new(program)
            .arg("review")
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("starting td-agent review: {e}"))?;
        let stdout = child.stdout.take().ok_or("no review output")?;
        let stderr = child.stderr.take().ok_or("no review errors")?;
        let out = std::thread::spawn(move || read_bounded(stdout));
        let err = std::thread::spawn(move || read_ends(stderr));
        let ended = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ended::Exited(status),
                Ok(None) => {}
                // Killed and reaped, and charged by its log all the same.
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Ended::Unwaited(e.to_string());
                }
            }
            if self.linger(POLL) {
                let _ = child.kill();
                let _ = child.wait();
                break Ended::Interrupted;
            }
        };
        let (review, cut) = out.join().unwrap_or_default();
        let (head, tail) = err.join().unwrap_or_default();
        Ok(Ran {
            ended,
            review,
            cut,
            head,
            tail,
        })
    }
}

/// `td-agent review`'s arguments for a review of `commit` in the staged
/// `repository` on `model`, capped at `cap`.
fn arguments(
    repository: &Path,
    commit: &str,
    model: &str,
    effort: Option<&str>,
    routing: Option<&str>,
    cap: u64,
) -> Vec<OsString> {
    let mut out: Vec<OsString> = vec![
        "--repo".into(),
        repository.as_os_str().to_os_string(),
        "--commit".into(),
        commit.into(),
        "--model".into(),
        model.into(),
        "--max-cost".into(),
        credits(cap).into(),
    ];
    if let Some(effort) = effort {
        out.extend(["--effort".into(), effort.into()]);
    }
    if let Some(routing) = routing {
        out.extend(["--routing".into(), routing.into()]);
    }
    out
}

/// `amount` as the exact decimal `--max-cost` reads.
fn credits(amount: u64) -> String {
    format!("{}.{:012}", amount / cost::ONE, amount % cost::ONE)
}

/// The review request's record of what was asked.
fn head(revision: &str, commit: &str, model: &str, cap: u64) -> String {
    let members = Json::Obj(vec![
        ("review".into(), Json::Str(revision.into())),
        ("commit".into(), Json::Str(commit.into())),
        ("model".into(), Json::Str(model.into())),
        ("max_cost".into(), Json::from(cap)),
    ])
    .to_string();
    members
        .strip_prefix('{')
        .and_then(|m| m.strip_suffix('}'))
        .unwrap_or_default()
        .to_string()
}

/// The session log the review's standard error named, if it began one.
fn session_log(stderr: &str) -> Option<PathBuf> {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix(SESSION_LOG))
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// What a review spent, as its session log says: nothing when it began
/// no log, or a log that holds no request, since each request's body is
/// logged before it is sent; what the log accounts otherwise; its cap
/// when the log cannot be read.
fn charge(stderr: &str, cap: u64) -> (u64, Basis) {
    let Some(path) = session_log(stderr) else {
        return (0, Basis::Nothing);
    };
    let summary = std::fs::File::open(&path)
        .ok()
        .and_then(|file| crate::review_metrics::summarize(file).ok());
    let Some(summary) = summary else {
        return (cap, Basis::Reserved);
    };
    if summary.get("requests").and_then(Json::as_u64) == Some(0) {
        return (0, Basis::Nothing);
    }
    match summary.get("accounted_cost").and_then(Json::as_u64) {
        Some(cost) => (cost, Basis::Reported),
        None => (cap, Basis::Reserved),
    }
}

/// At most `MAX_REVIEW` bytes of `input`, the rest read and dropped so
/// that the review never waits on a full pipe; and whether any was.
fn read_bounded(input: impl Read) -> (Vec<u8>, bool) {
    let mut input = input;
    let mut kept = Vec::new();
    let _ = (&mut input).take(MAX_REVIEW).read_to_end(&mut kept);
    let dropped = std::io::copy(&mut input, &mut std::io::sink()).unwrap_or(0);
    (kept, dropped > 0)
}

/// The start of `input`, for the session log's line, and its end, for
/// why a review failed.
fn read_ends(mut input: impl Read) -> (String, String) {
    let mut head: Vec<u8> = Vec::new();
    let mut tail: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = match input.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let bytes = chunk.get(..read).unwrap_or_default();
        if head.len() < MAX_STDERR_HEAD {
            let room = MAX_STDERR_HEAD - head.len();
            head.extend_from_slice(bytes.get(..room.min(read)).unwrap_or_default());
        }
        tail.extend_from_slice(bytes);
        if tail.len() > STDERR_TAIL * 2 {
            tail.drain(..tail.len() - STDERR_TAIL);
        }
    }
    if tail.len() > STDERR_TAIL {
        tail.drain(..tail.len() - STDERR_TAIL);
    }
    (
        String::from_utf8_lossy(&head).into_owned(),
        String::from_utf8_lossy(&tail).into_owned(),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The review is asked of the staged repository and commit, the
    /// model as given, its cap exactly, and effort and routing only when
    /// there are any.
    #[test]
    fn a_review_is_asked_of_td_agent_review_exactly() {
        let words = |arguments: Vec<OsString>| -> Vec<String> {
            arguments
                .into_iter()
                .map(|a| a.into_string().unwrap())
                .collect()
        };
        let commit = "c".repeat(40);
        assert_eq!(
            words(arguments(
                Path::new("/s/review.git"),
                &commit,
                "a/model",
                Some("high"),
                Some("cheapest"),
                cost::ONE / 4,
            )),
            [
                "--repo",
                "/s/review.git",
                "--commit",
                commit.as_str(),
                "--model",
                "a/model",
                "--max-cost",
                "0.250000000000",
                "--effort",
                "high",
                "--routing",
                "cheapest",
            ]
        );
        let plain = words(arguments(
            Path::new("/s"),
            &commit,
            "m",
            None,
            None,
            cost::ONE,
        ));
        assert_eq!(plain.len(), 8);
        assert_eq!(
            cost::parse(&credits(cost::ONE / 3), false),
            Some(cost::ONE / 3)
        );
        assert_eq!(
            head("HEAD", "abc", "m", 5),
            r#""review":"HEAD","commit":"abc","model":"m","max_cost":5"#
        );
        // Six times the most kept fits one log line.
        assert!(MAX_REVIEW.saturating_mul(6) + 64 * 1024 < crate::store::MAX_LINE as u64);
    }

    /// A review that named no session log, or whose log holds no
    /// request, asked no model and costs nothing; one whose log cannot
    /// be read costs its cap; one that asked, what its log accounts.
    #[test]
    fn a_review_is_charged_by_its_session_log() {
        assert_eq!(charge("error: no API key\n", 9), (0, Basis::Nothing));
        assert_eq!(
            session_log("td-agent review: session log /s/a.jsonl\nmore\n"),
            Some(PathBuf::from("/s/a.jsonl"))
        );
        // A tool's name is no session log's line.
        assert_eq!(session_log("td-agent review: tool session log /x\n"), None);
        assert_eq!(
            session_log("td-agent review: session log rel.jsonl\n"),
            None
        );
        assert_eq!(
            charge(
                "td-agent review: session log /nonexistent/td-agent-review.jsonl\n",
                9
            ),
            (9, Basis::Reserved)
        );
        let dir = std::env::temp_dir().join(format!(
            "td-agent-reviewing-{}",
            crate::store::random_hex(8).unwrap()
        ));
        std::fs::create_dir(&dir).unwrap();
        let line = |seq: u64, kind: &str, data: &str| {
            format!(r#"{{"sequence":{seq},"elapsed_ms":0,"kind":"{kind}","data":{data}}}"#)
        };
        let named = |path: &Path| format!("{SESSION_LOG}{}\n", path.display());
        // A review that failed before any request: nothing.
        let none = dir.join("none.jsonl");
        std::fs::write(&none, format!("{}\n", line(0, "start", r#"{"version":1}"#))).unwrap();
        assert_eq!(charge(&named(&none), 9), (0, Basis::Nothing));
        // One that asked: what the log accounts.
        let asked = dir.join("asked.jsonl");
        std::fs::write(
            &asked,
            [
                line(0, "start", r#"{"version":1}"#),
                line(1, "budget_reservation", r#"{"charged":0,"reserved":5}"#),
                line(2, "request_body", r#""{}""#),
                String::new(),
            ]
            .join("\n"),
        )
        .unwrap();
        // Its reservation, as the log accounts usage it never reported.
        assert_eq!(charge(&named(&asked), 9), (5, Basis::Reported));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The review's output is bounded and the rest drained; standard
    /// error keeps its start and its end.
    #[test]
    fn output_is_kept_within_its_bounds() {
        let long = vec![b'r'; MAX_REVIEW as usize + 10];
        let (kept, cut) = read_bounded(long.as_slice());
        assert_eq!((kept.len(), cut), (MAX_REVIEW as usize, true));
        let (kept, cut) = read_bounded(&b"fine"[..]);
        assert_eq!((kept.as_slice(), cut), (&b"fine"[..], false));
        let mut errors = String::from("td-agent review: session log /s/a.jsonl\n");
        errors.push_str(&"x".repeat(100_000));
        errors.push_str("\nthe end");
        let (head, tail) = read_ends(errors.as_bytes());
        assert!(head.starts_with("td-agent review: session log /s/a.jsonl\n"));
        assert_eq!(head.len(), MAX_STDERR_HEAD);
        assert!(tail.ends_with("\nthe end"));
        assert_eq!(tail.len(), STDERR_TAIL);
    }
}
