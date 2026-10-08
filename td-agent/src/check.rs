//! `td-agent check-jail`: the workspace jail as td-agent runs it, proved
//! once (DESIGN.md §8, "On td"). One instance is launched from
//! `TD_AGENT_JAIL` and `TD_AGENT_TXT`, as a conversation launches its
//! tools, with a private home and one worktree in a fresh directory under
//! the account's state directory; the tool host writes a file there, a
//! jailed shell finds git by name and reads the file back, and the host
//! sees what was written. The image's boot runs it (its `agent-evidence`
//! unit); its marker is the whole of what it prints.

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::host::{Call, Client, Done, Up};
use crate::jail::{self, Policy, Programs};

/// What a successful check prints, alone on its line.
pub const MARKER: &str = "TD-AGENT-JAIL-OK";
/// What the jailed write leaves, and the shell reads back.
const WRITTEN: &str = "written in the workspace jail\n";
/// How long the whole check may take: under the 60 seconds the image's
/// `agent-evidence` unit allows it, so it says why it failed and removes
/// its tree before td-svc would kill it.
pub const BUDGET: Duration = Duration::from_secs(45);
/// How long the jailed shell may run, within `BUDGET`.
const SHELL_MS: u64 = 20_000;

/// Runs the check with no arguments, printing `MARKER` when it holds.
pub fn run(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err("usage: td-agent check-jail".into());
    }
    let programs = Programs::from_env()?;
    let state = crate::store::StateDir::from_env(
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
    )?;
    let parent = state
        .root()
        .parent()
        .ok_or("the state directory has no parent")?
        .to_path_buf();
    std::fs::create_dir_all(&parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    // td-jail admits a worktree only by its canonical path, and td's
    // `/home` is a link.
    let parent =
        std::fs::canonicalize(&parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let base = parent.join(format!("td-agent-check-{}", crate::store::random_hex(8)?));
    let scratch = Scratch::new(base)?;
    let result = check(&programs, &scratch.0, Instant::now() + BUDGET);
    drop(scratch);
    result?;
    println!("{MARKER}");
    Ok(())
}

/// A fresh directory removed when the check ends, either way.
struct Scratch(PathBuf);
impl Scratch {
    fn new(path: PathBuf) -> Result<Self, String> {
        let mut private = std::fs::DirBuilder::new();
        private.mode(0o700);
        private
            .create(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let scratch = Self(path);
        for dir in ["tree", "jail"] {
            let path = scratch.0.join(dir);
            private
                .create(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(scratch)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn check(programs: &Programs, base: &Path, deadline: Instant) -> Result<(), String> {
    let tree = base.join("tree");
    let policy = Policy {
        home: base.join("jail/home"),
        worktrees: vec![tree.clone()],
        ..Policy::default()
    };
    let mut client = jail::launch(programs, &policy, &base.join("jail"))?;
    let file = tree.join("check.txt");
    done(
        &mut client,
        deadline,
        Call::Write {
            path: file.display().to_string(),
            content: WRITTEN.into(),
            expected: None,
        },
    )?;
    let on_host = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    if on_host != WRITTEN {
        return Err(format!("the jailed write left {on_host:?}"));
    }
    let shell = done(
        &mut client,
        deadline,
        Call::Shell {
            command: "command -v git && git --version && cat check.txt".into(),
            timeout_ms: Some(SHELL_MS),
            workdir: None,
        },
    )?;
    if !shell.text.contains("git version") || !shell.text.contains(WRITTEN.trim_end()) {
        return Err(format!("the jailed shell said {:?}", shell.text));
    }
    Ok(())
}

/// Call `call` and wait for its end until `deadline`, or say why not.
fn done(client: &mut Client, deadline: Instant, call: Call) -> Result<Done, String> {
    let want = client.call(call)?;
    while Instant::now() < deadline {
        match client.next_reply(Duration::from_millis(50)) {
            Some(Ok(Up::Done { id, outcome })) if id == want => return outcome,
            Some(Err(e)) => return Err(e),
            _ => {}
        }
    }
    Err(format!(
        "call {want} did not end; td-jail said {:?}",
        client.diagnostic()
    ))
}
