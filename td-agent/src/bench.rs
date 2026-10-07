//! A workspace conversation's tool instances (DESIGN.md §8, §12): the
//! policy its instances bind, worked out once; one long-lived instance
//! for the file tools, replaced when it fails or the policy changes; a
//! fresh one for each `shell`, `grep` and `sed`, so nothing a command
//! leaves outlives its call; and the digest of each file the
//! conversation last read or wrote, which a replacement must match.
//!
//! Every instance is started from the conversation's main thread, which
//! td-jail ties its lifetime to (`jail::launch`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::host::{Call, Client};
use crate::jail::{self, Policy, Programs};
use crate::shell;
use crate::store::{Event, Id, Kind, StateDir};
use crate::tools::{self, Args};
use crate::workspace::{self, Shared, Workspace};

/// What a conversation's tools are run with.
#[derive(Default)]
pub struct Bench {
    /// The jail's programs, found once; why there are none otherwise.
    programs: Option<Result<Programs, String>>,
    /// The policy and specs directory, for the shared directories and
    /// prepared repositories they were worked out with.
    policy: Option<(Vec<Shared>, Vec<PathBuf>, Policy, PathBuf)>,
    /// The file tools' instance, between calls.
    files: Option<Client>,
    digests: BTreeMap<String, String>,
}

/// Whether `call` runs in an instance of its own rather than the file
/// tools' (DESIGN.md §12).
pub fn fresh(call: &Call) -> bool {
    matches!(
        call,
        Call::Shell { .. }
            | Call::Background { .. }
            | Call::Grep { .. }
            | Call::Sed { .. }
            | Call::Snapshot { .. }
            | Call::Restore { .. }
    )
}

impl Bench {
    /// Works out the policy for `workspace` with `shared` and the
    /// repositories `prepared`, unless it is worked out for them already;
    /// a new one retires the file tools' instance, which binds the old
    /// (DESIGN.md §7: it is replaced when the ready worktrees change).
    pub fn prepare(
        &mut self,
        workspace: &Workspace,
        state: &StateDir,
        id: &Id,
        shared: &[Shared],
        prepared: &[PathBuf],
    ) -> Result<&Policy, String> {
        if self.programs.is_none() {
            self.programs = Some(Programs::from_env());
        }
        let current = self.policy.as_ref().is_some_and(|(with, ready, _, _)| {
            with.as_slice() == shared && ready.as_slice() == prepared
        });
        if !current {
            let (policy, specs) = workspace::policy(workspace, state, id, shared, prepared)?;
            self.files = None;
            self.policy = Some((shared.to_vec(), prepared.to_vec(), policy, specs));
        }
        self.policy
            .as_ref()
            .map(|(_, _, policy, _)| policy)
            .ok_or_else(|| "no workspace policy".to_string())
    }

    /// `call` with the digest a write or an edit must match: the one this
    /// conversation last read or wrote of its path, if it did.
    pub fn with_digest(&self, call: Call) -> Call {
        match call {
            Call::Write {
                path,
                content,
                expected: None,
            } => Call::Write {
                expected: self.digests.get(&key(&path)).cloned(),
                path,
                content,
            },
            Call::Edit {
                path,
                old,
                new,
                all,
                expected: None,
            } => Call::Edit {
                expected: self.digests.get(&key(&path)).cloned(),
                path,
                old,
                new,
                all,
            },
            other => other,
        }
    }

    /// The instance `call` runs in: a fresh one, or the file tools',
    /// started when there is none.
    pub fn take(&mut self, call: &Call) -> Result<Client, String> {
        if !fresh(call) {
            if let Some(files) = self.files.take() {
                return Ok(files);
            }
        }
        let programs = match &self.programs {
            Some(Ok(programs)) => programs,
            Some(Err(why)) => return Err(why.clone()),
            None => return Err("the workspace is not prepared".into()),
        };
        let (_, _, policy, specs) = self
            .policy
            .as_ref()
            .ok_or("the workspace is not prepared")?;
        jail::launch(programs, policy, specs)
    }

    /// Keeps the file tools' instance for the next call; a fresh one is
    /// dropped, which ends it.
    pub fn keep(&mut self, call: &Call, client: Client) {
        if !fresh(call) {
            self.files = Some(client);
        }
    }

    /// Notes the digest a read or write of `call`'s file left, which a
    /// later replacement must match.
    pub fn record(&mut self, call: &Call, digest: Option<&str>) {
        let path = match call {
            Call::Read { path, .. } | Call::Write { path, .. } | Call::Edit { path, .. } => path,
            _ => return,
        };
        if let Some(digest) = digest {
            self.digests.insert(key(path), digest.to_string());
        }
    }

    /// The digests a conversation's log holds, as a process starting
    /// again takes them up: each result that kept one, of the call its
    /// assistant message made.
    pub fn restore(&mut self, events: &[Event]) {
        for event in events {
            let Kind::ToolResult {
                reply,
                id,
                digest: Some(digest),
                error: false,
                ..
            } = &event.kind
            else {
                continue;
            };
            // The log is in sequence order.
            let call = events
                .binary_search_by_key(reply, |e| e.seq)
                .ok()
                .and_then(|at| events.get(at))
                .and_then(|e| match &e.kind {
                    Kind::Assistant { calls, .. } => calls.iter().find(|c| &c.id == id),
                    _ => None,
                });
            if let Some(Ok(Args::Host { call, .. })) =
                call.map(|c| tools::parse_in(tools::Kit::Workspace, &c.name, &c.arguments))
            {
                self.record(&call, Some(digest));
            }
        }
    }
}

/// The key a path's digest is kept under: its components, so `/w/./a`
/// and `/w//a` are `/w/a`; `..` is left for the tool host to judge.
fn key(path: &str) -> String {
    Path::new(path)
        .components()
        .collect::<PathBuf>()
        .to_string_lossy()
        .into_owned()
}

/// How long `call` may take before the conversation tears its jail down
/// (DESIGN.md §12): its own timeout, or a command's default, with room
/// for the tool host to say how it ended.
pub fn limit(call: &Call) -> Duration {
    let own = match call {
        Call::Shell {
            timeout_ms: Some(ms),
            ..
        } => Duration::from_millis(*ms),
        Call::Background { timeout_ms, .. } => timeout_ms
            .map_or(shell::MAX_BACKGROUND_TIMEOUT, Duration::from_millis)
            .min(shell::MAX_BACKGROUND_TIMEOUT),
        _ => shell::DEFAULT_TIMEOUT,
    };
    own.saturating_add(LIMIT_GRACE)
}

/// What a call's limit allows past its own time.
const LIMIT_GRACE: Duration = Duration::from_secs(15);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::store::Call as Asked;

    #[test]
    fn a_write_carries_the_digest_of_the_last_read() {
        let mut bench = Bench::default();
        let read = Call::Read {
            path: "/w/./a".into(),
            offset: None,
            limit: None,
        };
        bench.record(&read, Some("d1"));
        let write = |path: &str| Call::Write {
            path: path.into(),
            content: "x".into(),
            expected: None,
        };
        assert_eq!(
            bench.with_digest(write("/w//a")),
            Call::Write {
                path: "/w//a".into(),
                content: "x".into(),
                expected: Some("d1".into())
            }
        );
        assert_eq!(bench.with_digest(write("/w/b")), write("/w/b"));
        assert!(fresh(&Call::Shell {
            command: "true".into(),
            timeout_ms: None,
            workdir: None
        }));
        assert!(!fresh(&read));
        // Unprepared, nothing is launched.
        assert!(bench.take(&read).is_err());
        assert_eq!(
            limit(&Call::Shell {
                command: "x".into(),
                timeout_ms: Some(1000),
                workdir: None
            }),
            Duration::from_secs(16)
        );
        assert_eq!(limit(&read), shell::DEFAULT_TIMEOUT + LIMIT_GRACE);
        let background = |timeout_ms| Call::Background {
            command: "x".into(),
            timeout_ms,
            workdir: None,
        };
        assert!(fresh(&background(None)));
        assert_eq!(
            limit(&background(None)),
            shell::MAX_BACKGROUND_TIMEOUT + LIMIT_GRACE
        );
        assert_eq!(
            limit(&background(Some(u64::MAX))),
            shell::MAX_BACKGROUND_TIMEOUT + LIMIT_GRACE
        );
    }

    #[test]
    fn a_process_started_again_takes_the_digests_from_the_log() {
        let event = |seq: u64, kind: Kind| Event { seq, time: 0, kind };
        let asked = |id: &str, name: &str, arguments: &str| Asked {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        };
        let result = |reply: u64, id: &str, digest: Option<&str>, error: bool| Kind::ToolResult {
            reply,
            id: id.into(),
            name: String::new(),
            call: 0,
            content: String::new(),
            error,
            kept: None,
            digest: digest.map(str::to_string),
        };
        let events = [
            event(
                3,
                Kind::Assistant {
                    content: None,
                    reasoning: None,
                    details: None,
                    calls: vec![
                        asked("r", "read_file", r#"{"path":"/w/a"}"#),
                        asked("f", "read_file", r#"{"path":"/w/b"}"#),
                        asked(
                            "e",
                            "edit_file",
                            r#"{"path":"/w/a","old_string":"x","new_string":"y"}"#,
                        ),
                    ],
                    request: 2,
                    finish: "tool_calls".into(),
                    incomplete: false,
                },
            ),
            event(5, result(3, "r", Some("d1"), false)),
            event(6, result(3, "f", Some("bad"), true)),
            event(7, result(3, "e", Some("d2"), false)),
        ];
        let mut bench = Bench::default();
        bench.restore(&events);
        let edit = |path: &str| Call::Edit {
            path: path.into(),
            old: "y".into(),
            new: "z".into(),
            all: false,
            expected: None,
        };
        let expected = |call: Call| match call {
            Call::Edit { expected, .. } => expected,
            _ => None,
        };
        // The edit's digest, which came after the read's.
        assert_eq!(
            expected(bench.with_digest(edit("/w/a"))).as_deref(),
            Some("d2")
        );
        // A failed call's is not taken up.
        assert_eq!(expected(bench.with_digest(edit("/w/b"))), None);
    }
}
