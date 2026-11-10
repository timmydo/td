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
use crate::jail::{self, Policy, Programs, Unjailed};
use crate::shell;
use crate::store::{Event, Id, Kind, StateDir};
use crate::tools::{self, Args};
use crate::workspace::{self, Shared, Workspace};

/// What a conversation's tools are run with.
#[derive(Default)]
pub struct Bench {
    /// The jail's programs, found once; why there are none otherwise.
    programs: Option<Result<Programs, String>>,
    /// Whether the workspace is unconfined (DESIGN.md §8), its tool hosts
    /// started outside any jail with these programs.
    unconfined: bool,
    unjailed: Option<Result<Unjailed, String>>,
    /// The policy and specs directory, for the shared directories and
    /// prepared repositories they were worked out with.
    policy: Option<(Vec<Shared>, Vec<PathBuf>, Policy, PathBuf)>,
    /// The file tools' instance, between calls.
    files: Option<Client>,
    /// What a command's instance reaches the network by (DESIGN.md §10),
    /// shared by every instance and changed in place.
    judge: crate::egress::Judge,
    /// Whom a connection that waits for a decision is handed to.
    asker: Option<crate::egress::Asker>,
    digests: BTreeMap<String, String>,
}

/// Why an MCP server does not start in an unconfined workspace (DESIGN.md
/// §8, §12): a template's server is a standing decision made for a jail.
pub const UNCONFINED_SERVER: &str =
    "an MCP server does not start in an unconfined workspace, whose tools run outside any jail";

/// Whether `call` runs in an instance of its own rather than the file
/// tools' (DESIGN.md §12).
pub fn fresh(call: &Call) -> bool {
    matches!(
        call,
        Call::Shell { .. }
            | Call::Background { .. }
            | Call::Grep { .. }
            | Call::Sed { .. }
            | Call::Serve { .. }
    )
}

impl Bench {
    /// Works out the policy for `workspace` with `shared`, the
    /// repositories `prepared` and the Flathub `runtime` its template
    /// names, unless it is worked out for them already; a new one retires
    /// the file tools' instance, which binds the old (DESIGN.md §7: it is
    /// replaced when the ready worktrees change; §8, Runtime).
    pub fn prepare(
        &mut self,
        workspace: &Workspace,
        state: &StateDir,
        id: &Id,
        shared: &[Shared],
        prepared: &[PathBuf],
        runtime: Option<&str>,
    ) -> Result<&Policy, String> {
        if self.unconfined {
            // Only a jail binds one (DESIGN.md §8, Runtime); `take` refuses
            // it again.
            if let Some(runtime) = runtime {
                return Err(jail::unconfined_runtime(runtime));
            }
            if self.unjailed.is_none() {
                self.unjailed = Some(Unjailed::from_env());
            }
        } else if self.programs.is_none() {
            self.programs = Some(Programs::from_env());
        }
        let current = self
            .policy
            .as_ref()
            .is_some_and(|(with, ready, policy, _)| {
                with.as_slice() == shared
                    && ready.as_slice() == prepared
                    && policy.runtime.as_deref() == runtime
            });
        if !current {
            let (mut policy, specs) = workspace::policy(workspace, state, id, shared, prepared)?;
            policy.runtime = runtime.map(str::to_string);
            self.files = None;
            self.policy = Some((shared.to_vec(), prepared.to_vec(), policy, specs));
        }
        self.policy
            .as_ref()
            .map(|(_, _, policy, _)| policy)
            .ok_or_else(|| "no workspace policy".to_string())
    }

    /// Makes its tool hosts unconfined, as the conversation's workspace
    /// was made (DESIGN.md §8), before it is prepared.
    pub fn set_unconfined(&mut self, unconfined: bool) {
        self.unconfined = unconfined;
    }

    /// Whether its tool hosts run outside any jail.
    pub fn unconfined(&self) -> bool {
        self.unconfined
    }

    /// Whether `call` keeps to the workspace's roots (DESIGN.md §8,
    /// Unconfined workspaces): a file tool every path of which resolves,
    /// as it is now, inside the worktrees or the shared directories, and
    /// one that changes files inside the worktrees or the shared
    /// directories made writable. A command never does, nor does a path
    /// that cannot be told.
    pub fn contained(&self, call: &Call) -> bool {
        let Some((_, _, policy, _)) = &self.policy else {
            return false;
        };
        let canonical = |roots: Vec<PathBuf>| -> Vec<PathBuf> {
            roots
                .iter()
                .filter_map(|root| std::fs::canonicalize(root).ok())
                .collect()
        };
        let roots = canonical(policy.roots());
        let writable = canonical(policy.writable_roots());
        let inside = |path: &str| within(Path::new(path), &roots);
        // A repository's own `.git` is never changed unasked: its hooks
        // and configuration run in the next git command.
        let changes = |path: &str| {
            resolve(Path::new(path)).is_some_and(|resolved| {
                writable.iter().any(|root| {
                    resolved.strip_prefix(root).is_ok_and(|below| {
                        !below
                            .components()
                            .any(|c| c.as_os_str() == std::ffi::OsStr::new(".git"))
                    })
                })
            })
        };
        match call {
            Call::Read { path, .. } => inside(path),
            Call::Write { path, .. } | Call::Edit { path, .. } => changes(path),
            Call::Patch { patch, .. } => crate::patch::parse(patch)
                .is_ok_and(|parsed| parsed.paths().into_iter().all(changes)),
            Call::Glob { pattern, path } => {
                !pattern.split('/').any(|segment| segment == "..")
                    && path.as_deref().is_none_or(inside)
            }
            Call::Grep { path, .. } => path.as_deref().is_none_or(inside),
            Call::Sed { paths, .. } => paths.iter().all(|path| changes(path)),
            _ => false,
        }
    }

    /// The paths `call` reads or writes, as `contained` resolves them,
    /// each quoted, one a line; for a command, none (DESIGN.md §8). What
    /// does not exist yet is as the model wrote it, quoted so that it
    /// cannot make a line of its own.
    pub fn resolved(&self, call: &Call) -> Option<String> {
        let paths: Vec<String> = match call {
            Call::Read { path, .. } | Call::Write { path, .. } | Call::Edit { path, .. } => {
                vec![path.clone()]
            }
            Call::Patch { patch, .. } => crate::patch::parse(patch)
                .map(|parsed| parsed.paths().into_iter().map(str::to_string).collect())
                .unwrap_or_default(),
            Call::Sed { paths, .. } => paths.clone(),
            Call::Glob { path: None, .. } | Call::Grep { path: None, .. } => {
                return Some("(the workspace's directory, the pattern applied there)".into())
            }
            Call::Glob {
                path: Some(path), ..
            }
            | Call::Grep {
                path: Some(path), ..
            } => {
                vec![path.clone()]
            }
            _ => return None,
        };
        let lines: Vec<String> = paths
            .iter()
            .map(|path| match resolve(Path::new(path)) {
                Some(resolved) => format!("{:?}", resolved.display().to_string()),
                None => "(unresolved: relative, with `..`, a link that leads nowhere, or naming the resolving process)".into(),
            })
            .collect();
        Some(if lines.is_empty() {
            "(none: a patch that cannot be read)".to_string()
        } else {
            lines.join("\n")
        })
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
            // Each file the patch changes that this conversation read or
            // wrote, by the path as the patch spells it.
            Call::Patch { patch, expected } if expected.is_empty() => {
                let expected = crate::patch::parse(&patch)
                    .map(|parsed| {
                        parsed
                            .existing()
                            .into_iter()
                            .filter_map(|path| {
                                self.digests
                                    .get(&key(path))
                                    .map(|digest| (path.to_string(), digest.clone()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Call::Patch { patch, expected }
            }
            other => other,
        }
    }

    /// The instance `call`, logged as `started`, runs in: a fresh one, or
    /// the file tools', started when there is none.
    pub fn take(&mut self, call: &Call, _started: u64) -> Result<Client, String> {
        if !fresh(call) {
            if let Some(files) = self.files.take() {
                return Ok(files);
            }
        }
        if self.unconfined {
            if matches!(call, Call::Serve { .. }) {
                return Err(UNCONFINED_SERVER.into());
            }
            let programs = match &self.unjailed {
                Some(Ok(programs)) => programs,
                Some(Err(why)) => return Err(why.clone()),
                None => return Err("the workspace is not prepared".into()),
            };
            let (_, _, policy, _) = self
                .policy
                .as_ref()
                .ok_or("the workspace is not prepared")?;
            return jail::launch_unjailed(programs, policy);
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

    /// The web-fetch policy; command and MCP networking is host networking.
    pub fn set_network(
        &mut self,
        network: crate::config::Network,
        allowlist: Vec<crate::config::Destination>,
        relay: Option<PathBuf>,
    ) {
        if let Ok(mut egress) = self.judge.lock() {
            egress.network = network;
            egress.allowlist = allowlist;
            egress.relay = relay;
        }
    }

    /// The network rules a connection is judged by, and the rules files
    /// that could not be read (DESIGN.md §11).
    pub fn set_rules(&mut self, rules: Vec<crate::rules::Sourced>, unread: Vec<(String, String)>) {
        if let Ok(mut egress) = self.judge.lock() {
            egress.rules = rules;
            egress.unread = unread;
        }
    }

    /// What an instance launched now is launched with, which an MCP
    /// server is started again when it changes (DESIGN.md §12, Lifetime).
    pub fn launch(&self) -> crate::servers::Launch {
        self.policy
            .as_ref()
            .map(|(shared, prepared, policy, _)| {
                (shared.clone(), prepared.clone(), policy.runtime.clone())
            })
            .unwrap_or_default()
    }

    /// The web-fetch policy and allowlist supplied to its classifier.
    pub fn network(&self) -> (crate::config::Network, Vec<crate::config::Destination>) {
        self.judge.lock().map_or_else(
            |_| (crate::config::Network::Off, Vec::new()),
            |egress| (egress.network, egress.allowlist.clone()),
        )
    }

    /// Whom a connection that waits is handed to.
    pub fn set_asker(&mut self, asker: crate::egress::Asker) {
        self.asker = Some(asker);
    }

    /// What a connection to `destination` would be, by the current
    /// policy and rules.
    pub fn judge(&self, destination: &crate::config::Destination) -> crate::egress::Judgment {
        self.judge.lock().map_or_else(
            |_| crate::egress::Judgment::Refuse("the policy is not readable".into()),
            |egress| egress.judge(destination),
        )
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

    /// Notes a patch's digests: each file it wrote with its digest now,
    /// and each it deleted or moved away forgotten.
    pub fn record_each(&mut self, digests: &[(String, Option<String>)]) {
        for (path, digest) in digests {
            match digest {
                Some(digest) => self.digests.insert(key(path), digest.clone()),
                None => self.digests.remove(&key(path)),
            };
        }
    }

    /// Puts `digests` in place of the ones a write must match, and gives
    /// those back: a sub-agent's are its own, from none, so a write of
    /// the conversation's never matches a version only the sub-agent saw
    /// (DESIGN.md §12, `task`).
    pub fn swap_digests(&mut self, digests: BTreeMap<String, String>) -> BTreeMap<String, String> {
        std::mem::replace(&mut self.digests, digests)
    }

    /// The digests a conversation's log holds, as a process starting
    /// again takes them up: each result that kept one, of the call its
    /// assistant message made; never a sub-agent's, whose replies answer
    /// no turn request.
    pub fn restore(&mut self, events: &[Event]) {
        let mut turns: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
        let mut replies: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
        for event in events {
            match &event.kind {
                Kind::Request {
                    purpose: crate::store::Purpose::Turn,
                    ..
                } => {
                    turns.insert(event.seq);
                }
                Kind::Assistant { request, .. } if turns.contains(request) => {
                    replies.insert(event.seq);
                }
                _ => {}
            }
            let Kind::ToolResult {
                reply,
                id,
                digest,
                digests,
                error: false,
                ..
            } = &event.kind
            else {
                continue;
            };
            if !replies.contains(reply) {
                continue;
            }
            self.record_each(digests);
            let Some(digest) = digest else {
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

/// Whether `path`, absolute, resolves inside one of `roots`, which are
/// canonical: its nearest existing ancestor as it resolves, with the
/// components that do not exist yet after it. A path with a `..`
/// anywhere, or a link that leads nowhere yet, cannot be told, and is
/// not; nor is one written under a tree naming the resolving process.
fn within(path: &Path, roots: &[PathBuf]) -> bool {
    resolve(path).is_some_and(|resolved| roots.iter().any(|root| resolved.starts_with(root)))
}

/// `path` as `within` resolves it, or none where it cannot be told.
fn resolve(path: &Path) -> Option<PathBuf> {
    // What these name is the resolving process's own, which the tool host
    // is not; and with no `..`, none is reached but as written.
    if !path.is_absolute() || PROCESS_OWN.iter().any(|own| path.starts_with(own)) {
        return None;
    }
    let mut existing = PathBuf::new();
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => return None,
            std::path::Component::Normal(name) if !rest.is_empty() => rest.push(name),
            other => {
                let next = existing.join(other);
                match std::fs::symlink_metadata(&next) {
                    Ok(_) => existing = next,
                    Err(_) => match other {
                        std::path::Component::Normal(name) => rest.push(name),
                        _ => return None,
                    },
                }
            }
        }
    }
    let mut resolved = std::fs::canonicalize(&existing).ok()?;
    resolved.extend(rest);
    Some(resolved)
}

/// Trees whose paths name the process that resolves them.
const PROCESS_OWN: &[&str] = &[
    "/proc",
    "/dev/fd",
    "/dev/stdin",
    "/dev/stdout",
    "/dev/stderr",
];

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

    /// An unconfined workspace's call keeps to its roots only when every
    /// path it names resolves inside one (DESIGN.md §8): through a link
    /// or `..` out of them it does not, nor does a command, ever.
    #[test]
    fn an_unconfined_call_is_contained_only_inside_its_roots() {
        let owned = td_fs::TempDir::new("contained").unwrap();
        let base = std::fs::canonicalize(owned.path()).unwrap();
        let work = base.join("work");
        let outside = base.join("outside");
        std::fs::create_dir(&work).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(work.join("a"), "a").unwrap();
        std::fs::write(outside.join("s"), "s").unwrap();
        std::os::unix::fs::symlink(outside.join("s"), work.join("link")).unwrap();
        std::os::unix::fs::symlink(outside.join("nowhere"), work.join("dangling")).unwrap();
        std::os::unix::fs::symlink(&outside, work.join("away")).unwrap();
        let mut bench = Bench::default();
        let text = |path: &Path| path.display().to_string();
        let read = |path: &Path| Call::Read {
            path: text(path),
            offset: None,
            limit: None,
        };
        let write = |path: &Path| Call::Write {
            path: text(path),
            content: "x".into(),
            expected: None,
        };
        // Unprepared, nothing is.
        assert!(!bench.contained(&read(&work.join("a"))));
        bench.policy = Some((
            Vec::new(),
            Vec::new(),
            Policy {
                worktrees: vec![work.clone()],
                ..Policy::default()
            },
            base.join("specs"),
        ));
        assert!(bench.contained(&read(&work.join("a"))));
        assert!(bench.contained(&read(&work.join("./a"))));
        assert!(bench.contained(&write(&work.join("new/deeper"))));
        assert!(bench.contained(&read(&work)));
        assert!(!bench.contained(&read(&outside.join("s"))));
        assert!(!bench.contained(&read(&work.join("link"))));
        assert!(!bench.contained(&write(&work.join("dangling"))));
        assert!(!bench.contained(&write(&work.join("away/new"))));
        assert!(!bench.contained(&read(&work.join("../outside/s"))));
        assert!(!bench.contained(&write(&work.join("new/../../outside/x"))));
        assert!(!bench.contained(&read(Path::new("a"))));
        assert!(!bench.contained(&read(&base)));
        assert!(!bench.contained(&read(Path::new("/proc/self/cwd/a"))));
        assert!(!bench.contained(&read(Path::new("/dev/fd/3"))));
        assert!(!bench.contained(&read(&work.join("../work/a"))));
        // A repository's `.git` is read, never changed, unasked: its hooks
        // run in the next git command.
        std::fs::create_dir_all(work.join(".git/hooks")).unwrap();
        std::os::unix::fs::symlink(work.join(".git"), work.join("meta")).unwrap();
        assert!(bench.contained(&read(&work.join(".git/config"))));
        assert!(!bench.contained(&write(&work.join(".git/hooks/pre-commit"))));
        assert!(!bench.contained(&write(&work.join("meta/config"))));
        assert!(!bench.contained(&write(&work.join("sub/.git/config"))));
        // The classifier is told where a call's paths resolve.
        assert_eq!(
            bench.resolved(&read(&work.join("link"))),
            Some(format!("{:?}", text(&outside.join("s"))))
        );
        // What the model wrote cannot make a line of its own.
        assert_eq!(
            bench.resolved(&write(&work.join("new\n/etc/passwd"))),
            Some(format!("{:?}", text(&work.join("new\n/etc/passwd"))))
        );
        assert!(!bench
            .resolved(&write(&work.join("new\n/etc/passwd")))
            .is_some_and(|line| line.contains('\n')));
        assert_eq!(
            bench.resolved(&read(Path::new("a"))).as_deref(),
            Some("(unresolved: relative, with `..`, a link that leads nowhere, or naming the resolving process)")
        );
        assert_eq!(
            bench.resolved(&Call::Shell {
                command: "true".into(),
                timeout_ms: None,
                workdir: None,
            }),
            None
        );
        // A directory shared read-only is read, never changed, unasked.
        let notes = base.join("notes");
        std::fs::create_dir(&notes).unwrap();
        std::fs::write(notes.join("n"), "n").unwrap();
        if let Some((_, _, policy, _)) = bench.policy.as_mut() {
            policy.read = vec![notes.clone()];
        }
        assert!(bench.contained(&read(&notes.join("n"))));
        assert!(!bench.contained(&write(&notes.join("n"))));
        assert!(!bench.contained(&write(&notes.join("new"))));
        if let Some((_, _, policy, _)) = bench.policy.as_mut() {
            policy.read = Vec::new();
            policy.write = vec![notes.clone()];
        }
        assert!(bench.contained(&write(&notes.join("n"))));
        if let Some((_, _, policy, _)) = bench.policy.as_mut() {
            policy.write = Vec::new();
        }
        let glob = |pattern: &str, path: Option<&Path>| Call::Glob {
            pattern: pattern.into(),
            path: path.map(text),
        };
        assert!(bench.contained(&glob("**/*.rs", None)));
        assert!(bench.contained(&glob("*", Some(&work))));
        assert!(!bench.contained(&glob("../*", None)));
        assert!(!bench.contained(&glob("*", Some(&outside))));
        let grep = |path: Option<&Path>| Call::Grep {
            pattern: "x".into(),
            path: path.map(text),
            include: None,
            exclude: None,
            extended: false,
            ignore_case: false,
            context: None,
        };
        assert!(bench.contained(&grep(None)));
        assert!(!bench.contained(&grep(Some(&outside))));
        let sed = |paths: &[&Path]| Call::Sed {
            script: "s/a/b/".into(),
            paths: paths.iter().map(|path| text(path)).collect(),
            extended: false,
        };
        assert!(bench.contained(&sed(&[&work.join("a")])));
        assert!(!bench.contained(&sed(&[&work.join("a"), &outside.join("s")])));
        let patch = |text: String| Call::Patch {
            patch: text,
            expected: Vec::new(),
        };
        let a = text(&work.join("a"));
        assert!(bench.contained(&patch(format!(
            "*** Begin Patch\n*** Update File: {a}\n@@\n-a\n+b\n*** End Patch\n"
        ))));
        let moved = text(&outside.join("moved"));
        assert!(!bench.contained(&patch(format!(
            "*** Begin Patch\n*** Update File: {a}\n*** Move to: {moved}\n@@\n-a\n+b\n*** End Patch\n"
        ))));
        assert!(!bench.contained(&patch("not a patch".into())));
        assert!(!bench.contained(&Call::Shell {
            command: "true".into(),
            timeout_ms: None,
            workdir: None
        }));
        // An MCP server does not start outside the jail.
        bench.set_unconfined(true);
        bench.unjailed = Some(Ok(Unjailed {
            agent: "/bin/true".into(),
            txt: None,
        }));
        let serve = Call::Serve {
            program: "server".into(),
            args: Vec::new(),
            workdir: None,
        };
        assert_eq!(bench.take(&serve, 1).err().unwrap(), UNCONFINED_SERVER);
    }

    #[test]
    fn web_fetch_settings_do_not_restart_servers() {
        let mut bench = Bench::default();
        let launch = bench.launch();
        bench.set_network(crate::config::Network::Open, Vec::new(), None);
        assert_eq!(launch, bench.launch());
        bench.set_network(crate::config::Network::Off, Vec::new(), None);
        assert_eq!(launch, bench.launch());
    }

    /// A template's runtime is in the policy and in what servers are
    /// launched with, and a change works the policy out again
    /// (DESIGN.md §8, Runtime); an unconfined workspace runs none.
    #[test]
    fn a_runtime_is_the_policys_and_never_an_unconfined_hosts() {
        let dir = td_fs::TempDir::new("td-agent-bench-runtime").unwrap();
        let state = StateDir::at(dir.path().to_path_buf());
        let id = Id::parse(&"0".repeat(32)).unwrap();
        let sdk = Some("freedesktop-sdk-25-08");
        let mut bench = Bench::default();
        let policy = bench
            .prepare(&Workspace::Scratch, &state, &id, &[], &[], sdk)
            .unwrap();
        assert_eq!(policy.runtime.as_deref(), sdk);
        assert!(policy.development);
        assert_eq!(bench.launch().2.as_deref(), sdk);
        let native = bench
            .prepare(&Workspace::Scratch, &state, &id, &[], &[], None)
            .unwrap();
        assert_eq!(native.runtime, None);
        assert_eq!(bench.launch().2, None);
        // Were a runtime to reach an unconfined workspace, nothing of it
        // would run there: preparing it is refused, and a policy naming
        // one is refused again at launch.
        bench.set_unconfined(true);
        let refused = bench
            .prepare(&Workspace::Scratch, &state, &id, &[], &[], sdk)
            .err()
            .unwrap();
        assert_eq!(refused, jail::unconfined_runtime("freedesktop-sdk-25-08"));
        bench
            .prepare(&Workspace::Scratch, &state, &id, &[], &[], None)
            .unwrap();
        if let Some((_, _, policy, _)) = bench.policy.as_mut() {
            policy.runtime = sdk.map(str::to_string);
        }
        bench.unjailed = Some(Ok(Unjailed {
            agent: "/bin/true".into(),
            txt: None,
        }));
        let shell = Call::Shell {
            command: "true".into(),
            timeout_ms: None,
            workdir: None,
        };
        let refused = bench.take(&shell, 1).err().unwrap();
        assert!(refused.contains("unconfined workspace"), "{refused}");
        assert!(refused.contains("freedesktop-sdk-25-08"), "{refused}");
    }

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
        assert!(bench.take(&read, 1).is_err());
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
            digests: Vec::new(),
            pictures: Vec::new(),
        };
        let request = |purpose, prefix| Kind::Request {
            turn: 1,
            purpose,
            prefix,
            head: String::new(),
            bytes: 0,
            reserved: 0,
        };
        let events = [
            event(2, request(crate::store::Purpose::Turn, 0)),
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
            // A sub-agent's edit of the same file is its own.
            event(9, request(crate::store::Purpose::Task, 8)),
            event(
                10,
                Kind::Assistant {
                    content: None,
                    reasoning: None,
                    details: None,
                    calls: vec![asked(
                        "s",
                        "edit_file",
                        r#"{"path":"/w/a","old_string":"y","new_string":"q"}"#,
                    )],
                    request: 9,
                    finish: "tool_calls".into(),
                    incomplete: false,
                },
            ),
            event(11, result(10, "s", Some("d3"), false)),
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
        // The edit's digest, which came after the read's, and not the
        // sub-agent's after it.
        assert_eq!(
            expected(bench.with_digest(edit("/w/a"))).as_deref(),
            Some("d2")
        );
        // A failed call's is not taken up.
        assert_eq!(expected(bench.with_digest(edit("/w/b"))), None);

        // A patch: it carries the digest of each file it changes that was
        // read, and what it wrote and deleted is noted, here and from the
        // log alike.
        let patch = |text: &str| Call::Patch {
            patch: text.into(),
            expected: Vec::new(),
        };
        let changes = "*** Begin Patch\n*** Update File: /w/./a\n-y\n+z\n*** Delete File: /w/b\n*** Add File: /w/n\n+x\n*** End Patch\n";
        assert_eq!(
            bench.with_digest(patch(changes)),
            Call::Patch {
                patch: changes.into(),
                expected: vec![("/w/./a".into(), "d2".into())],
            }
        );
        let mut patched = Bench::default();
        patched.record(&edit("/w/b"), Some("d0"));
        let mut events = events.to_vec();
        events.push(event(15, request(crate::store::Purpose::Turn, 0)));
        events.push(event(
            16,
            Kind::Assistant {
                content: None,
                reasoning: None,
                details: None,
                calls: vec![asked("p", "apply_patch", r#"{"input":"x"}"#)],
                request: 15,
                finish: "tool_calls".into(),
                incomplete: false,
            },
        ));
        let mut done = result(16, "p", None, false);
        if let Kind::ToolResult { digests, .. } = &mut done {
            *digests = vec![("/w/a".into(), Some("d3".into())), ("/w/b".into(), None)];
        }
        events.push(event(17, done));
        patched.restore(&events);
        assert_eq!(
            expected(patched.with_digest(edit("/w/a"))).as_deref(),
            Some("d3")
        );
        assert_eq!(expected(patched.with_digest(edit("/w/b"))), None);
    }
}
