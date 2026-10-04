//! The tool host as a process of the built program (DESIGN.md §2, §12):
//! `td-agent tool-host` serves calls over its standard input and output,
//! and ends when its input does, cancelling what still runs. The jail it
//! will run in is a later increment's (§8, §18).
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

use td_agent::frame;
use td_agent::host::{Call, Client, Done, Down, Up};

const PROGRAM: &str = env!("CARGO_BIN_EXE_td-agent");

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "td-agent-tool-host-{tag}-{}-{}",
            std::process::id(),
            td_agent::store::random_hex(4).unwrap()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn done(client: &mut Client, want: u64) -> Result<Done, String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        match client.next_reply(Duration::from_millis(50)) {
            Some(Ok(Up::Done { id, outcome })) if id == want => return outcome,
            Some(Err(e)) => panic!("{e}"),
            _ => {}
        }
    }
    panic!("call {want} did not end")
}

#[test]
fn the_tool_host_serves_calls_as_a_process() {
    let scratch = Scratch::new("serve");
    let mut client =
        Client::spawn(Path::new(PROGRAM), std::slice::from_ref(&scratch.0), None).unwrap();
    let path = scratch.0.join("f.txt").display().to_string();
    let id = client
        .call(Call::Write {
            path: path.clone(),
            content: "hello\n".into(),
            expected: None,
        })
        .unwrap();
    let written = done(&mut client, id).unwrap();
    let id = client
        .call(Call::Shell {
            command: "cat f.txt".into(),
            timeout_ms: Some(10_000),
            workdir: None,
        })
        .unwrap();
    assert_eq!(
        done(&mut client, id).unwrap().text,
        "[exit status 0]\nhello\n"
    );
    let id = client
        .call(Call::Read {
            path,
            offset: None,
            limit: None,
        })
        .unwrap();
    assert_eq!(done(&mut client, id).unwrap().digest, written.digest);
}

/// Its input closing ends it, and the calls it was running with it.
#[test]
fn the_tool_host_ends_with_its_input() {
    let scratch = Scratch::new("end");
    let mut child = Command::new(PROGRAM)
        .args(["tool-host", "--root"])
        .arg(&scratch.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    let call = Down::Call {
        id: 1,
        call: Call::Shell {
            command: "echo running; exec sleep 60".into(),
            timeout_ms: None,
            workdir: None,
        },
    };
    frame::write(&mut input, &call.encode()).unwrap();
    let first = Up::decode(&frame::read(&mut output).unwrap().unwrap()).unwrap();
    assert_eq!(
        first,
        Up::Output {
            id: 1,
            text: "running\n".into()
        }
    );
    let closed = Instant::now();
    drop(input);
    // The cancelled call still ends, then the host does.
    let last = Up::decode(&frame::read(&mut output).unwrap().unwrap()).unwrap();
    let Up::Done {
        id: 1,
        outcome: Ok(done),
    } = last
    else {
        panic!("{last:?}")
    };
    assert!(done.text.starts_with("[interrupted"), "{}", done.text);
    assert!(child.wait().unwrap().success());
    assert!(closed.elapsed() < Duration::from_secs(10));
}

/// grep and sed through a real td-txt, named by `TD_AGENT_TXT`; the gate
/// has no td-txt binary to give td-agent's tests, which cover the argv
/// mapping instead.
#[test]
#[ignore = "needs TD_AGENT_TXT naming a built td-txt"]
fn grep_and_sed_run_td_txt() {
    let txt = PathBuf::from(std::env::var_os("TD_AGENT_TXT").expect("TD_AGENT_TXT"));
    let scratch = Scratch::new("txt");
    let file = scratch.0.join("src/a.rs");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "fn old() {}\nfn other() {}\n").unwrap();
    std::fs::write(scratch.0.join("notes.txt"), "old\n").unwrap();
    let mut client = Client::spawn(
        Path::new(PROGRAM),
        std::slice::from_ref(&scratch.0),
        Some(&txt),
    )
    .unwrap();
    let id = client
        .call(Call::Grep {
            pattern: "fn o[a-z]*".into(),
            path: None,
            include: Some("*.rs".into()),
            exclude: None,
            extended: false,
            ignore_case: false,
            context: None,
        })
        .unwrap();
    let found = done(&mut client, id).unwrap().text;
    assert!(found.contains("src/a.rs:1:fn old() {}"), "{found}");
    assert!(found.contains("src/a.rs:2:fn other() {}"), "{found}");
    assert!(!found.contains("notes.txt"), "{found}");
    let id = client
        .call(Call::Sed {
            script: "s/old/new/".into(),
            paths: vec![file.display().to_string()],
            extended: false,
        })
        .unwrap();
    done(&mut client, id).unwrap();
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "fn new() {}\nfn other() {}\n"
    );
    // --sandbox: a script that would write another file is refused.
    let id = client
        .call(Call::Sed {
            script: format!("w {}/stolen", scratch.0.display()),
            paths: vec![file.display().to_string()],
            extended: false,
        })
        .unwrap();
    let refused = done(&mut client, id).unwrap().text;
    assert!(refused.contains("sandbox"), "{refused}");
    assert!(!scratch.0.join("stolen").exists());
    let id = client
        .call(Call::Grep {
            pattern: "absent".into(),
            path: None,
            include: None,
            exclude: None,
            extended: false,
            ignore_case: false,
            context: None,
        })
        .unwrap();
    assert_eq!(done(&mut client, id).unwrap().text, "[no matches]\n");
}
