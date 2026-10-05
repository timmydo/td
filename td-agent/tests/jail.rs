//! The tool host in a real workspace jail (DESIGN.md §8): td-jail's
//! `workspace` kind, launched as a conversation launches it. Ignored by
//! default: it needs unprivileged user namespaces and a built td-jail and
//! td-txt, named by `TD_AGENT_JAIL` and `TD_AGENT_TXT` as td-net's launch
//! names them. Run with `cargo test --test jail -- --ignored`.
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use td_agent::host::{Call, Client, Done, Up};
use td_agent::jail::{self, Policy, Programs};
use td_agent::repo::{self, Identity, Task, Worktree};

const PROGRAM: &str = env!("CARGO_BIN_EXE_td-agent");
/// Set in a child of the lifetime test, which then launches and waits.
const LAUNCHER_VAR: &str = "TD_AGENT_TEST_JAIL_LAUNCHER";
/// Names the lifetime test's long command, after the scratch tree.
const WORKLOAD: &str = "/lifetime-workload";

/// A scratch tree outside `/tmp`, which td-jail reserves.
struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "td-agent-jail-{tag}-{}-{}",
            std::process::id(),
            td_agent::store::random_hex(4).unwrap()
        ));
        for dir in ["tree", "shared", "jail"] {
            std::fs::create_dir_all(path.join(dir)).unwrap();
        }
        Self(std::fs::canonicalize(path).unwrap())
    }

    fn policy(&self) -> Policy {
        Policy {
            home: self.0.join("jail/home"),
            worktrees: vec![self.0.join("tree")],
            read: vec![self.0.join("shared")],
            ..Policy::default()
        }
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn programs() -> Programs {
    let named = |var: &str| PathBuf::from(std::env::var_os(var).unwrap_or_else(|| panic!("{var}")));
    Programs::new(
        named(jail::JAIL_VAR),
        PathBuf::from(PROGRAM),
        named(jail::TXT_VAR),
    )
    .unwrap()
}

fn done(client: &mut Client, call: Call) -> Result<Done, String> {
    let want = client.call(call).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        match client.next_reply(Duration::from_millis(50)) {
            Some(Ok(Up::Done { id, outcome })) if id == want => return outcome,
            Some(Err(e)) => panic!("{e}"),
            _ => {}
        }
    }
    panic!(
        "call {want} did not end; td-jail said {:?}",
        client.diagnostic()
    )
}

fn shell(client: &mut Client, command: &str) -> String {
    done(
        client,
        Call::Shell {
            command: command.into(),
            timeout_ms: Some(20_000),
            workdir: None,
        },
    )
    .unwrap()
    .text
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn the_jailed_tool_host_works_inside_its_policy() {
    let scratch = Scratch::new("serve");
    std::fs::write(scratch.0.join("shared/given.txt"), "from the human\n").unwrap();
    let mut client = jail::launch(&programs(), &scratch.policy(), &scratch.0.join("jail")).unwrap();
    let tree = scratch.0.join("tree");
    // The host's git, where a bound tree holds it, is found by name, as
    // the model's shell asks for it.
    if let Some(git) = bound_git() {
        let found = shell(&mut client, "command -v git");
        assert_eq!(
            found
                .lines()
                .last()
                .and_then(|line| std::fs::canonicalize(line).ok()),
            Some(git),
            "{found}"
        );
    }

    let file = tree.join("a.rs").display().to_string();
    done(
        &mut client,
        Call::Write {
            path: file.clone(),
            content: "fn old() {}\n".into(),
            expected: None,
        },
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(tree.join("a.rs")).unwrap(),
        "fn old() {}\n"
    );
    let given = scratch.0.join("shared/given.txt").display().to_string();
    let read = done(
        &mut client,
        Call::Read {
            path: given.clone(),
            offset: None,
            limit: None,
        },
    )
    .unwrap();
    assert!(read.text.contains("from the human"), "{}", read.text);
    // The shared directory is read-only, by the mount and not by td-agent.
    let refused = done(
        &mut client,
        Call::Write {
            path: given,
            content: "changed\n".into(),
            expected: read.digest,
        },
    );
    assert!(refused.is_err(), "{refused:?}");

    // td-txt runs inside, as the program the spec bound.
    let found = done(
        &mut client,
        Call::Grep {
            pattern: "old".into(),
            path: None,
            include: None,
            exclude: None,
            extended: false,
            ignore_case: false,
            context: None,
        },
    )
    .unwrap();
    assert!(found.text.contains("a.rs:1:fn old() {}"), "{}", found.text);

    let inside = shell(
        &mut client,
        "echo cwd=$(pwd); echo home=$HOME; grep -E '^(Seccomp|NoNewPrivs):' /proc/self/status; \
         ls /proc | grep -c '^[0-9]'; touch /usr/x 2>/dev/null || echo usr=read-only; \
         ls -A \"$HOME/..\" | head -3; echo built > built && echo tree=writable",
    );
    for line in [
        format!("cwd={}", tree.display()),
        format!("home={}", scratch.0.join("jail/home").display()),
        "Seccomp:\t2".into(),
        "NoNewPrivs:\t1".into(),
        "usr=read-only".into(),
        "tree=writable".into(),
    ] {
        assert!(inside.contains(&line), "{line} absent from {inside}");
    }
    // The caller's home is absent: the scratch tree's spec directory, a
    // sibling of the jail home, is not there.
    let spec_listing = shell(&mut client, "ls -A \"$HOME/..\"");
    assert!(!spec_listing.contains("spec-"), "{spec_listing}");
    assert!(std::fs::read_to_string(tree.join("built"))
        .unwrap()
        .starts_with("built"));
    // Of the caller's home, only the way to the granted trees is there.
    let real_home = PathBuf::from(std::env::var("HOME").unwrap());
    let hidden = shell(
        &mut client,
        &format!("ls -A {} 2>&1 | sed 's/^/entry=/'", real_home.display()),
    );
    let entries: Vec<_> = hidden
        .lines()
        .filter_map(|line| line.strip_prefix("entry="))
        .collect();
    match scratch.0.strip_prefix(&real_home) {
        Ok(rest) => {
            let first = rest.components().next().unwrap().as_os_str();
            assert_eq!(entries, [first.to_str().unwrap()], "{hidden}");
        }
        Err(_) => assert!(
            entries.len() == 1 && entries[0].contains("No such file"),
            "{hidden}"
        ),
    }
    // No Unix socket but a stream pair.
    let socket = shell(
        &mut client,
        "command -v python3 >/dev/null && python3 -c 'import socket; socket.socket(socket.AF_UNIX)' \
         2>&1 | tail -1 || echo no-python",
    );
    assert!(
        socket.contains("PermissionError") || socket.contains("no-python"),
        "{socket}"
    );
    drop(client);
    let left: Vec<_> = std::fs::read_dir(scratch.0.join("jail"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("spec-"))
        .collect();
    assert!(left.is_empty(), "the spec outlived its instance");
}

/// Plain git, the test's own, for the upstream and the store.
fn plain(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "protocol.file.allow=always",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// The host's git as an instance sees it: where `git` on PATH resolves,
/// when that is a system tree the jail binds.
fn bound_git() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let found = std::env::split_paths(&path)
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file())?;
    let resolved = std::fs::canonicalize(found).ok()?;
    ["/usr/", "/bin/", "/gnu/", "/nix/"]
        .iter()
        .any(|tree| resolved.starts_with(tree))
        .then_some(resolved)
}

/// A workspace repository as td-agent lays it out over a store, checked
/// out by a maintenance instance and committed to by a jailed shell
/// (DESIGN.md §8, §9), with every protected file out of the model's
/// reach.
#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL, TD_AGENT_TXT and a host git"]
fn a_maintenance_instance_checks_out_and_a_jailed_git_commits() {
    let Some(git) = bound_git() else {
        panic!("no git on PATH in a tree the jail binds");
    };
    let scratch = Scratch::new("repo");
    let up = scratch.0.join("up");
    for path in ["a/x", "c/d/z", "c/w", "top"] {
        let file = up.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, format!("{path}\n")).unwrap();
    }
    plain(&up, &["init", "--quiet"]);
    plain(&up, &["add", "."]);
    plain(&up, &["commit", "--quiet", "-m", "one"]);
    let store = scratch.0.join("store/up.git");
    std::fs::create_dir_all(&store).unwrap();
    plain(&store, &["init", "--quiet", "--bare"]);
    let from = up.display().to_string();
    plain(
        &store,
        &["fetch", "--quiet", &from, "+refs/heads/*:refs/heads/*"],
    );
    let base = plain(&store, &["rev-parse", "main"]).trim().to_string();

    let repository = scratch.0.join("ws/w/r.git");
    let checkout = scratch.0.join("trees/w/r");
    let identity = Identity {
        name: Some("Human".into()),
        email: Some("h@example.org".into()),
    };
    repo::create(&repository, &store, &identity).unwrap();
    repo::add_worktree(
        &repository,
        &Worktree {
            id: "r".into(),
            checkout: checkout.clone(),
            branch: "agent/one".into(),
            sparse: Some(vec!["c/d".into()]),
        },
    )
    .unwrap();
    let policy = |home: &str| Policy {
        home: scratch.0.join(home),
        checkouts: vec![checkout.clone()],
        repositories: vec![repository.clone()],
        objects: vec![store.join("objects")],
        ..Policy::default()
    };
    let task = Task::Checkout {
        git: git.clone(),
        repository: repository.clone(),
        id: "r".into(),
        checkout: checkout.clone(),
        branch: "agent/one".into(),
        base: base.clone(),
    };
    let specs = scratch.0.join("jail");
    let time = Duration::from_secs(60);
    assert_eq!(
        jail::maintain(
            &programs(),
            &policy("jail/maintenance"),
            &specs,
            &task,
            time
        )
        .unwrap(),
        format!("agent/one checked out at {base}")
    );
    for (path, there) in [
        ("top", true),
        ("c/w", true),
        ("c/d/z", true),
        ("a/x", false),
    ] {
        assert_eq!(checkout.join(path).exists(), there, "{path}");
    }
    // The worktree is checked out once: a second run is refused, and says
    // why.
    let refused = jail::maintain(
        &programs(),
        &policy("jail/maintenance"),
        &specs,
        &task,
        time,
    )
    .unwrap_err();
    assert!(refused.contains("is not checked out again"), "{refused}");

    // The model's own git, in a shell instance: it commits as the human
    // the repository names, and cannot touch what the chain protects.
    let mut client = jail::launch(&programs(), &policy("jail/home"), &specs).unwrap();
    let git = git.display();
    let said = shell(
        &mut client,
        &format!(
            "cd {checkout} && echo more >> top && {git} commit -q -a -m two && \
             {git} log --format=log=%an:%s -1; \
             echo x >> {repository}/config 2>/dev/null || echo config=read-only; \
             {git} sparse-checkout add a && test -e a/x && echo add=widened; \
             {git} sparse-checkout set c/d && test ! -e a/x && echo set=narrowed; \
             {git} sparse-checkout list | sed 's/^/list=/'; \
             {git} sparse-checkout set --cone a 2>/dev/null || echo cone=refused; \
             {git} sparse-checkout set --no-cone a 2>/dev/null || echo no-cone=refused; \
             {git} sparse-checkout list | sed 's/^/kept=/'",
            checkout = checkout.display(),
            repository = repository.display(),
        ),
    );
    for line in [
        "log=Human:two",
        "config=read-only",
        "add=widened",
        "set=narrowed",
        "list=c/d",
        "cone=refused",
        "no-cone=refused",
        "kept=c/d",
    ] {
        assert!(
            said.lines().any(|got| got == line),
            "{line} absent from {said}"
        );
    }
    drop(client);
    assert!(!said.lines().any(|got| got == "list=a"), "{said}");
    let tip = std::fs::read_to_string(repository.join("refs/heads/agent/one")).unwrap();
    assert_ne!(tip.trim(), base, "the commit moved the branch");
    assert_eq!(
        std::fs::read_to_string(repository.join("config")).unwrap(),
        repo::config_text(&repository, &identity).unwrap()
    );
}

/// A repository conversation prepares its workspace as the window
/// answers its ask (DESIGN.md §7): the repository laid out, its worktree
/// checked out in a maintenance instance, then recorded prepared and its
/// tools bound to it.
#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL, TD_AGENT_TXT and a host git"]
fn a_repository_conversation_prepares_its_workspace() {
    use td_agent::protocol::{Down, Fetched, Up};
    use td_agent::store::{Conversation, Id, Kind, Role, StateDir};
    use td_agent::supervisor::{Supervisor, Update};
    assert!(
        bound_git().is_some(),
        "no git on PATH in a tree the jail binds"
    );
    let scratch = Scratch::new("prepare");
    let up = scratch.0.join("up");
    for path in ["a/x", "top"] {
        let file = up.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, format!("{path}\n")).unwrap();
    }
    plain(&up, &["init", "--quiet"]);
    plain(&up, &["add", "."]);
    plain(&up, &["commit", "--quiet", "-m", "one"]);
    let id = Id::random().unwrap();
    let template = td_agent::config::Template {
        name: "td".into(),
        repos: vec![td_agent::config::Repo {
            remote: "https://example.org/a/td".into(),
            base: "main".into(),
            branch: "agent".into(),
            sparse: Some(vec!["a".into()]),
        }],
        shared: None,
    };
    let made = td_agent::workspace::repositories(
        &template,
        &id,
        &scratch.0.join("data"),
        &scratch.0.join("trees"),
        &[td_agent::git::Admission::parse("example.org").unwrap()],
        0,
    )
    .unwrap();
    let entry = made.entries.first().unwrap().clone();
    // The store as the window's worker leaves it.
    std::fs::create_dir_all(&entry.store).unwrap();
    plain(&entry.store, &["init", "--quiet", "--bare"]);
    let from = up.display().to_string();
    plain(
        &entry.store,
        &["fetch", "--quiet", &from, "+refs/heads/*:refs/heads/*"],
    );
    let base = plain(&entry.store, &["rev-parse", "main"])
        .trim()
        .to_string();
    let state = StateDir::at(scratch.0.join("state"));
    state.ensure().unwrap();
    let keyless = Down::Setup {
        key: Err("no API key".into()),
        client: td_agent::config::Client::default(),
    };
    let named = |var: &str| std::env::var_os(var).unwrap_or_else(|| panic!("{var}"));
    let mut supervisor = Supervisor::new(PROGRAM.into(), state.root().to_path_buf(), keyless)
        .env(jail::JAIL_VAR, named(jail::JAIL_VAR))
        .env(jail::TXT_VAR, named(jail::TXT_VAR));
    supervisor
        .create(
            id.clone(),
            Role::Conversation,
            td_agent::workspace::Workspace::Repositories(made),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut asked = false;
    let mut ready = None;
    // Ready, it asks where its bases are now, so a move while it
    // checked out is not missed.
    let mut followed = false;
    while ready.is_none() || !followed {
        assert!(Instant::now() < deadline, "no notice came, or no Heads");
        for (_, update) in supervisor.poll() {
            match update {
                Update::Up(Up::Fetch { remote, bases }) if !asked => {
                    assert_eq!(bases, ["main"]);
                    asked = true;
                    supervisor.answer(
                        &id,
                        &Down::Fetched {
                            remote,
                            result: Ok(Fetched {
                                identity: Identity {
                                    name: Some("Human".into()),
                                    email: Some("h@example.org".into()),
                                },
                                ids: vec![base.clone()],
                                instructions: vec![td_agent::repo::Instructions::Found {
                                    name: "AGENTS.md".into(),
                                    text: "Read the docs.\n".into(),
                                }],
                            }),
                        },
                    );
                }
                Update::Up(Up::Event(event)) => {
                    if let Kind::Notification { text } = event.kind {
                        ready = Some(text);
                    }
                }
                Update::Up(Up::Heads { bases, .. }) => {
                    assert!(ready.is_some(), "asked before it was ready");
                    assert_eq!(bases, ["main"]);
                    followed = true;
                }
                // The window keeps the process until the checkout is done.
                Update::Up(Up::Prepared { .. }) => {
                    assert!(ready.is_some(), "done with before it was ready");
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let said = ready.unwrap();
    assert!(said.contains("is checked out and ready"), "{said}");
    assert!(entry.checkout.join("a/x").is_file() && entry.checkout.join("top").is_file());
    drop(supervisor);
    let (conversation, _) = Conversation::open(&state, &id, None, Duration::from_secs(5)).unwrap();
    assert_eq!(
        conversation.meta().prepared,
        std::slice::from_ref(&entry.repository)
    );
    // The instructions the answer carried are recorded for the worktree.
    let recorded = conversation.instructions();
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded.first().map(|r| (&r.checkout, &r.base)),
        Some((&entry.checkout, &base))
    );
    // The base's remote-tracking ref is set where the worktree started,
    // and recorded.
    let origin = || {
        plain(
            &entry.repository,
            &["rev-parse", "refs/remotes/origin/main"],
        )
        .trim()
        .to_string()
    };
    assert_eq!(origin(), base);
    let tracked = |conversation: &Conversation| {
        conversation
            .meta()
            .tracked
            .iter()
            .map(|t| (t.remote.clone(), t.base.clone(), t.id.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        tracked(&conversation),
        [(entry.remote.clone(), "main".to_string(), base.clone())]
    );
    drop(conversation);
    // Upstream moves; started again, the process asks where its base is,
    // and told, sets the ref there and says so.
    plain(&up, &["commit", "--quiet", "--allow-empty", "-m", "two"]);
    plain(
        &entry.store,
        &["fetch", "--quiet", &from, "+refs/heads/*:refs/heads/*"],
    );
    let moved = plain(&entry.store, &["rev-parse", "main"])
        .trim()
        .to_string();
    let keyless = Down::Setup {
        key: Err("no API key".into()),
        client: td_agent::config::Client::default(),
    };
    let mut supervisor = Supervisor::new(PROGRAM.into(), state.root().to_path_buf(), keyless)
        .env(jail::JAIL_VAR, named(jail::JAIL_VAR))
        .env(jail::TXT_VAR, named(jail::TXT_VAR));
    supervisor.open(id.clone(), None).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut said = None;
    while said.is_none() {
        assert!(Instant::now() < deadline, "no notice came");
        for (_, update) in supervisor.poll() {
            match update {
                Update::Up(Up::Fetch { .. }) => panic!("a prepared repository fetched again"),
                Update::Up(Up::Heads { remote, bases }) => {
                    assert_eq!(bases, ["main"]);
                    supervisor.answer(
                        &id,
                        &Down::Heads {
                            remote,
                            bases,
                            ids: vec![moved.clone()],
                        },
                    );
                }
                Update::Up(Up::Event(event)) => {
                    // Its log's older notices come again as it opens.
                    match event.kind {
                        Kind::Notification { text } if text.contains("upstream") => {
                            said = Some(text)
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let said = said.unwrap();
    assert!(
        said.contains(&format!(
            "main moved from {} to {}",
            &base[..12],
            &moved[..12]
        )),
        "{said}"
    );
    assert_eq!(origin(), moved);
    drop(supervisor);
    let (conversation, _) = Conversation::open(&state, &id, None, Duration::from_secs(5)).unwrap();
    assert_eq!(
        tracked(&conversation),
        [(entry.remote.clone(), "main".to_string(), moved.clone())]
    );
    drop(conversation);
    // Asked before a deletion, the worktree reports nothing to lose,
    // then a change and a commit; removed, the tree and the repository
    // go and the store stays (DESIGN.md §7).
    let programs = || {
        jail::Programs::new(
            named(jail::JAIL_VAR).into(),
            PROGRAM.into(),
            named(jail::TXT_VAR).into(),
        )
    };
    let made = match state.workspace(&id).unwrap() {
        Some(td_agent::workspace::Workspace::Repositories(made)) => made,
        other => panic!("{other:?}"),
    };
    // A step snapshot in the conversation's own kind of instance: the
    // worktree kept as a tree on its snapshot ref, and a change named
    // (DESIGN.md §12).
    let checkouts = vec![entry.checkout.display().to_string()];
    let git_path = td_agent::repo::host_git().unwrap().display().to_string();
    let host = |call: td_agent::host::Call| {
        let workspace = td_agent::workspace::Workspace::Repositories(made.clone());
        let (policy, specs) = td_agent::workspace::policy(
            &workspace,
            &state,
            &id,
            &[],
            std::slice::from_ref(&entry.repository),
        )
        .unwrap();
        let mut client = jail::launch(&programs().unwrap(), &policy, &specs).unwrap();
        client.call(call).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            assert!(Instant::now() < deadline, "no snapshot came");
            if let Some(reply) = client.next_reply(Duration::from_millis(100)) {
                if let td_agent::host::Up::Done { outcome, .. } = reply.unwrap() {
                    let text = outcome.unwrap_or_else(|why| panic!("{why}")).text;
                    break td_agent::snapshot::decode(&text, &checkouts).unwrap();
                }
            }
        }
    };
    let snap = |before: &[String]| {
        host(td_agent::host::Call::Snapshot {
            git: git_path.clone(),
            checkouts: checkouts.clone(),
            before: before.to_vec(),
        })
    };
    let restore = |from: &str, to: &str| {
        host(td_agent::host::Call::Restore {
            git: git_path.clone(),
            checkouts: checkouts.clone(),
            from: vec![from.to_string()],
            to: vec![to.to_string()],
        })
    };
    let first = snap(&[]);
    let kept = format!("refs/td-agent/snapshots/{}", entry.id);
    assert_eq!(
        plain(
            &entry.repository,
            &["rev-parse", &format!("{kept}^{{tree}}")]
        )
        .trim(),
        first[0].tree
    );
    std::fs::write(entry.checkout.join("a/new"), "made in a step\n").unwrap();
    let second = snap(&[first[0].tree.clone()]);
    assert_eq!(second[0].changed, ["a/new"]);
    assert_eq!(
        plain(
            &entry.repository,
            &["rev-parse", &format!("{kept}^{{tree}}")]
        )
        .trim(),
        second[0].tree
    );
    // Undone and redone there too.
    let undone = restore(&second[0].tree, &first[0].tree);
    assert_eq!(undone[0].changed, ["a/new"]);
    assert!(!entry.checkout.join("a/new").exists());
    restore(&first[0].tree, &second[0].tree);
    assert_eq!(
        std::fs::read_to_string(entry.checkout.join("a/new")).unwrap(),
        "made in a step\n"
    );
    restore(&second[0].tree, &first[0].tree);
    assert!(!entry.checkout.join("a/new").exists());
    let asked = || td_agent::removal::survey(&state, &id, &made, programs());
    let found = asked();
    assert_eq!(found.len(), 1);
    assert!(td_agent::removal::lost(&found).is_empty(), "{found:?}");
    std::fs::write(entry.checkout.join("a/new"), "new\n").unwrap();
    let lost = td_agent::removal::lost(&asked());
    assert_eq!(lost.len(), 1);
    assert!(
        lost.iter()
            .all(|l| l.contains("1 changed or untracked file")),
        "{lost:?}"
    );
    let git = bound_git().unwrap();
    let committed = std::process::Command::new(&git)
        .current_dir(&entry.checkout)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.org",
            "commit",
            "--quiet",
            "-am",
            "x",
        ])
        .status()
        .unwrap();
    assert!(
        !committed.success(),
        "nothing tracked changed: the new file is untracked"
    );
    let added = std::process::Command::new(&git)
        .current_dir(&entry.checkout)
        .args(["add", "a/new"])
        .status()
        .unwrap();
    assert!(added.success());
    let committed = std::process::Command::new(&git)
        .current_dir(&entry.checkout)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.org",
            "commit",
            "--quiet",
            "-m",
            "x",
        ])
        .status()
        .unwrap();
    assert!(committed.success());
    let lost = td_agent::removal::lost(&asked());
    assert!(
        lost.iter()
            .all(|l| l.contains("1 commit, in its repository")),
        "{lost:?}"
    );
    td_agent::removal::doom(&made).finish().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while entry
        .repository
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .read_dir()
        .unwrap()
        .next()
        .is_some()
        || made
            .tree()
            .unwrap()
            .parent()
            .unwrap()
            .read_dir()
            .unwrap()
            .next()
            .is_some()
    {
        assert!(Instant::now() < deadline, "the workspace was not removed");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(entry.store.join("objects").is_dir(), "the store stays");
}

/// The instance dies with the process that launched it, killed with
/// `SIGKILL` and so with no chance to clean up (§8).
#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn a_killed_launcher_leaves_no_instance() {
    let scratch = Scratch::new("kill");
    let marker = scratch.0.display().to_string();
    let mut launcher = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "launcher_holds_an_instance",
            "--ignored",
            "--nocapture",
        ])
        .env(LAUNCHER_VAR, &scratch.0)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    {
        use std::io::BufRead;
        let mut lines = std::io::BufReader::new(launcher.stdout.take().unwrap());
        while !ready.contains("instance-ready") {
            ready.clear();
            assert_ne!(lines.read_line(&mut ready).unwrap(), 0, "no instance");
        }
    }
    // td-jail's outer process, stage 1, stage 2, the tool host and the
    // command it runs, which alone names the workload.
    let before = processes_naming(&marker);
    assert!(
        before.len() >= 5 && before.iter().any(|(_, line)| line.contains(WORKLOAD)),
        "{before:?}"
    );
    launcher.kill().unwrap();
    launcher.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut after = processes_naming(&marker);
    while !after.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        after = processes_naming(&marker);
    }
    assert!(after.is_empty(), "instance processes survived: {after:?}");
}

/// The lifetime test's launcher: an instance running a long command,
/// then nothing until it is killed.
#[test]
#[ignore = "run by a_killed_launcher_leaves_no_instance"]
fn launcher_holds_an_instance() {
    let Some(root) = std::env::var_os(LAUNCHER_VAR) else {
        return;
    };
    let scratch = Scratch(PathBuf::from(root));
    let mut client = jail::launch(&programs(), &scratch.policy(), &scratch.0.join("jail")).unwrap();
    let marker = format!("{}{WORKLOAD}", scratch.0.display());
    // A compound command, so the shell running it stays and carries the
    // marker in its command line.
    client
        .call(Call::Shell {
            command: format!("sleep 600; : {marker}"),
            timeout_ms: Some(600_000),
            workdir: None,
        })
        .unwrap();
    // Until it runs, so the instance is whole when it is killed.
    let deadline = Instant::now() + Duration::from_secs(20);
    while processes_naming(&marker).is_empty() {
        assert!(
            Instant::now() < deadline,
            "the workload never ran: {:?}",
            client.diagnostic()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("instance-ready");
    std::thread::sleep(Duration::from_secs(600));
    drop((client, scratch));
}

/// Processes whose command line names `marker`, this one excepted.
fn processes_naming(marker: &str) -> Vec<(u32, String)> {
    let me = std::process::id();
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().filter_map(Result::ok) {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let line = String::from_utf8_lossy(&raw).replace('\0', " ");
        if line.contains(marker) && !line.contains("launcher_holds_an_instance") {
            found.push((pid, line));
        }
    }
    found
}
