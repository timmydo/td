//! The tool host in a real workspace jail (DESIGN.md §8): td-jail's
//! `workspace` kind, launched as a conversation launches it. Ignored by
//! default: it needs unprivileged user namespaces and a built td-jail and
//! td-txt, named by `TD_AGENT_JAIL` and `TD_AGENT_TXT` as `./agent` names
//! them. Run with `cargo test --test jail -- --ignored`.
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
            write: Vec::new(),
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
