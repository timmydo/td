//! The tool host (DESIGN.md §2): this program under the `tool-host`
//! personality, which performs every tool effect, inside a jail instance
//! (§8). It reads `host::Down` frames from its input and writes
//! `host::Up` frames to its output, each call on a thread of its own so
//! they run side by side, at most `host::MAX_CALLS` at once. It ends when
//! its input does, cancelling what still runs.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};

use crate::files;
use crate::frame;
use crate::host::{Call, Done, Down, Up, MAX_CALLS, OUTPUT_CHUNK};
use crate::shell::{self, Grep, Sed};

/// What a tool host serves: the worktrees, the first being where a call
/// names no directory unless `directory` names it, and td-txt for `grep`
/// and `sed`.
#[derive(Clone, Debug, Default)]
pub struct Config {
    pub roots: Vec<PathBuf>,
    pub txt: Option<PathBuf>,
    /// Where a call that names no directory works, when that is not
    /// the first root: a repository workspace's first worktree, which
    /// may not be bound yet.
    pub directory: Option<PathBuf>,
    /// Whether this instance serves the network proxy (DESIGN.md §10),
    /// and its commands are pointed at it.
    pub proxy: bool,
}

impl Config {
    /// `--root DIR` (repeated), `--txt PATH` and `--directory DIR`, each
    /// absolute, and `--proxy`.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let usage = "usage: td-agent tool-host [--proxy] [--txt ABSOLUTE-PATH] [--directory ABSOLUTE-DIR] [--root ABSOLUTE-DIR]...";
        let mut config = Self::default();
        let mut rest = args.iter();
        while let Some(flag) = rest.next() {
            if flag == "--proxy" {
                config.proxy = true;
                continue;
            }
            let value = PathBuf::from(rest.next().ok_or(usage)?);
            if !value.is_absolute() {
                return Err(format!("{} is not an absolute path", value.display()));
            }
            match flag.as_str() {
                "--root" => config.roots.push(value),
                "--txt" => config.txt = Some(value),
                "--directory" => config.directory = Some(value),
                _ => return Err(usage.into()),
            }
        }
        Ok(config)
    }

    /// Where a call that names no directory works: the first worktree,
    /// refused while it is not bound rather than another root in its
    /// place.
    fn first(&self, what: &str) -> Result<PathBuf, String> {
        if let Some(directory) = &self.directory {
            return self
                .roots
                .contains(directory)
                .then(|| directory.clone())
                .ok_or_else(|| {
                    format!(
                        "the working directory, {}, is not checked out yet; give `{what}` as an absolute path in a worktree that is",
                        directory.display()
                    )
                });
        }
        self.roots.first().cloned().ok_or_else(|| {
            format!("this workspace has no worktree; give `{what}` as an absolute path")
        })
    }

    fn path(&self, given: &str, name: &str) -> Result<PathBuf, String> {
        files::absolute(given, name, &self.roots)
    }

    fn txt(&self, tool: &str) -> Result<&Path, String> {
        self.txt
            .as_deref()
            .ok_or_else(|| format!("{tool} is not available here: this tool host has no td-txt"))
    }
}

/// The calls running, by id, each with its cancel flag.
type Running = Arc<Mutex<BTreeMap<u64, Arc<AtomicBool>>>>;

/// Each running `serve` call's input, by id, until its input closes or
/// it ends. Unbounded, since the conversation sends at most
/// `host::INPUT_WINDOW` past what the program took.
type Inputs = Arc<Mutex<BTreeMap<u64, mpsc::Sender<Vec<u8>>>>>;

/// Lets a `serve` call's input go when its thread ends, a panic included.
struct Forget {
    inputs: Inputs,
    id: u64,
}

impl Drop for Forget {
    fn drop(&mut self) {
        if let Ok(mut inputs) = self.inputs.lock() {
            inputs.remove(&self.id);
        }
    }
}

/// The most frames waiting for the writer. Foreground display updates
/// may be dropped; background and review output waits for room. Review
/// deadlines also run in the controller, which can tear down the jail.
const OUTBOX: usize = 256;
pub(crate) const REVIEW_CARGO_OFFLINE: &str = "true";

/// Writes one reply. A call's end too large for a frame is sent as an
/// error instead, so the conversation still learns that the call ended;
/// a reply that cannot be written is dropped, since the conversation
/// that would read it is gone.
fn reply(output: &mut impl Write, up: &Up) {
    let mut bytes = up.encode();
    if let (true, Up::Done { id, .. }) = (bytes.len() > frame::MAX_FRAME, up) {
        let why = format!(
            "the result was {} bytes, past the {}-byte frame; ask for less",
            bytes.len(),
            frame::MAX_FRAME
        );
        bytes = Up::Done {
            id: *id,
            outcome: Err(why),
        }
        .encode();
    }
    let _ = frame::write(output, &bytes);
}

/// A call's end, sent when its thread finishes however it finishes, so
/// its slot is freed and its caller answered even if it panicked.
struct Finish {
    id: u64,
    running: Running,
    outbox: SyncSender<Up>,
    outcome: Option<Result<Done, String>>,
}

impl Finish {
    fn end(&mut self, outcome: Result<Done, String>) {
        self.outcome = Some(outcome);
    }
}

impl Drop for Finish {
    fn drop(&mut self) {
        if let Ok(mut running) = self.running.lock() {
            running.remove(&self.id);
        }
        let outcome = self
            .outcome
            .take()
            .unwrap_or_else(|| Err("the call failed inside the tool host".into()));
        let _ = self.outbox.send(Up::Done {
            id: self.id,
            outcome,
        });
    }
}

/// Serves calls from `input` until it ends.
pub fn serve(
    mut input: impl Read,
    mut output: impl Write + Send + 'static,
    config: Config,
) -> Result<(), String> {
    let (outbox, frames) = mpsc::sync_channel::<Up>(OUTBOX);
    // Before any call, so a command never runs pointed at a proxy that is
    // not there.
    let proxy = match config.proxy {
        true => Some(crate::proxy::start(outbox.clone(), crate::proxy::PORT)?.0),
        false => None,
    };
    // One writer, so a call's output frames come before its end.
    let writer = std::thread::Builder::new()
        .spawn(move || {
            for up in frames {
                reply(&mut output, &up);
            }
        })
        .map_err(|e| format!("the tool host's writer: {e}"))?;
    let running: Running = Arc::new(Mutex::new(BTreeMap::new()));
    let inputs: Inputs = Arc::new(Mutex::new(BTreeMap::new()));
    let config = Arc::new(config);
    let mut threads = Vec::new();
    let ended = loop {
        let bytes = match frame::read(&mut input) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => break Ok(()),
            Err(e) => break Err(format!("the conversation's frames: {e}")),
        };
        match Down::decode(&bytes) {
            Ok(Down::Link(link)) => {
                if let Some(proxy) = &proxy {
                    proxy.down(link);
                }
            }
            Ok(Down::Cancel { id }) => {
                if let Some(flag) = running.lock().ok().and_then(|r| r.get(&id).cloned()) {
                    flag.store(true, Ordering::Relaxed);
                }
            }
            // Handed to the call's own writer, never written here, so a
            // program that stops reading holds up nothing else.
            Ok(Down::Input { id, data }) => {
                if let Some(input) = inputs.lock().ok().and_then(|i| i.get(&id).cloned()) {
                    let _ = input.send(data);
                }
            }
            Ok(Down::Close { id }) => {
                if let Ok(mut inputs) = inputs.lock() {
                    inputs.remove(&id);
                }
            }
            Ok(Down::Call { id, call }) => {
                let flag = Arc::new(AtomicBool::new(false));
                let refusal = match running.lock() {
                    // Answering would end the running call for its caller.
                    Ok(running) if running.contains_key(&id) => continue,
                    Ok(running) if running.len() >= MAX_CALLS => Some(format!(
                        "{MAX_CALLS} calls are already running; wait for one to end"
                    )),
                    Ok(mut running) => {
                        running.insert(id, flag.clone());
                        None
                    }
                    Err(_) => Some("the tool host's call table is broken".into()),
                };
                if let Some(why) = refusal {
                    let _ = outbox.send(Up::Done {
                        id,
                        outcome: Err(why),
                    });
                    continue;
                }
                let mut finish = Finish {
                    id,
                    running: running.clone(),
                    outbox: outbox.clone(),
                    outcome: None,
                };
                let config = config.clone();
                // A serve call's input is open before any of it can come.
                let input = match call {
                    Call::Serve { .. } => {
                        let (send, receive) = mpsc::channel();
                        if let Ok(mut inputs) = inputs.lock() {
                            inputs.insert(id, send);
                        }
                        Some(receive)
                    }
                    _ => None,
                };
                let let_go = inputs.clone();
                // A thread the system refuses drops `finish` unrun, which
                // answers the call as failed.
                // A background call's output is all kept (DESIGN.md §12),
                // so it waits for room; a call's is shown as it comes, and
                // what does not fit is dropped.
                let whole = matches!(call, Call::Background { .. });
                let spawned = std::thread::Builder::new().spawn(move || {
                    let live = finish.outbox.clone();
                    if let Some(input) = input {
                        // Its input let go however the call ends.
                        let _forget = Forget { inputs: let_go, id };
                        finish.end(serve_program(&call, &config, &flag, input, id, &live));
                        return;
                    }
                    if matches!(call, Call::ReviewShell { .. }) {
                        // Review traces need every byte; bounded queues apply
                        // backpressure rather than dropping display updates.
                        finish.end(act(&call, &config, &flag, &mut |chunk| {
                            for bytes in chunk.chunks(crate::host::OUTPUT_CHUNK) {
                                if live
                                    .send(Up::RawOutput {
                                        id,
                                        bytes: bytes.to_vec(),
                                    })
                                    .is_err()
                                {
                                    flag.store(true, Ordering::Relaxed);
                                    break;
                                }
                            }
                        }));
                        return;
                    }
                    finish.end(perform(&call, &config, &flag, &mut |text| {
                        if whole {
                            let _ = live.send(Up::Output { id, text });
                        } else {
                            let _ = live.try_send(Up::Output { id, text });
                        }
                    }));
                });
                match spawned {
                    Ok(thread) => threads.push(thread),
                    Err(_) => {
                        if let Ok(mut inputs) = inputs.lock() {
                            inputs.remove(&id);
                        }
                    }
                }
                threads.retain(|thread| !thread.is_finished());
            }
            Err(e) => {
                // A call whose arguments are wrong is refused as its own
                // error; a frame that names no call ends the host.
                let id = td_json::parse_slice(&bytes)
                    .ok()
                    .and_then(|value| value.get("call").and_then(td_json::Json::as_u64));
                let Some(id) = id else {
                    break Err(format!("a frame from the conversation: {e}"));
                };
                if running
                    .lock()
                    .map_or(true, |running| running.contains_key(&id))
                {
                    continue;
                }
                let _ = outbox.send(Up::Done {
                    id,
                    outcome: Err(e),
                });
            }
        }
    };
    cancel_all(&running);
    if let Ok(mut inputs) = inputs.lock() {
        inputs.clear();
    }
    for thread in threads {
        let _ = thread.join();
    }
    if let Some(proxy) = &proxy {
        proxy.stop();
    }
    drop(outbox);
    let _ = writer.join();
    ended
}

fn cancel_all(running: &Running) {
    if let Ok(running) = running.lock() {
        for flag in running.values() {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

/// Runs a `serve` call (DESIGN.md §12, MCP servers): its input from
/// `input`, each piece's taking said; its output as raw bytes and its
/// standard error as text, each waiting for room in `live` rather than
/// dropped, since a lost piece would break the program's protocol.
fn serve_program(
    call: &Call,
    config: &Config,
    cancel: &AtomicBool,
    input: mpsc::Receiver<Vec<u8>>,
    id: u64,
    live: &SyncSender<Up>,
) -> Result<Done, String> {
    let Call::Serve {
        program,
        args,
        workdir,
    } = call
    else {
        return Err("not a serve call".into());
    };
    let dir = match workdir {
        Some(dir) => config.path(dir, "workdir")?,
        None => config.first("workdir")?,
    };
    if !dir.is_dir() {
        return Err(format!("`workdir` {} is not a directory", dir.display()));
    }
    let mut errors = Vec::new();
    let exit = shell::serve(
        shell::program(program, args, &dir, config.proxy),
        cancel,
        input,
        &mut |bytes| {
            let _ = live.send(Up::Took { id, bytes });
        },
        &mut |stream, piece| match stream {
            shell::Stream::Output => {
                for bytes in piece.chunks(OUTPUT_CHUNK) {
                    let _ = live.send(Up::RawOutput {
                        id,
                        bytes: bytes.to_vec(),
                    });
                }
            }
            shell::Stream::Errors => {
                errors.extend_from_slice(piece);
                emit(&mut errors, false, &mut |text| {
                    let _ = live.send(Up::Output { id, text });
                });
            }
        },
    )?;
    emit(&mut errors, true, &mut |text| {
        let _ = live.send(Up::Output { id, text });
    });
    let mut text = exit.status();
    if exit.cut {
        text.push_str(shell::CUT_NOTE);
    }
    Ok(Done {
        text,
        kept: None,
        digest: None,
        digests: Vec::new(),
    })
}

/// Performs one call, giving `live` its process's output as it comes.
pub fn perform(
    call: &Call,
    config: &Config,
    cancel: &AtomicBool,
    live: &mut dyn FnMut(String),
) -> Result<Done, String> {
    let mut pending = Vec::new();
    let outcome = act(call, config, cancel, &mut |chunk| {
        pending.extend_from_slice(chunk);
        emit(&mut pending, false, live);
    });
    emit(&mut pending, true, live);
    outcome
}

/// Hands `live` the output in `pending` as text, at most `OUTPUT_CHUNK`
/// bytes a piece and each cut between characters; a character not yet
/// whole waits for its next bytes, unless this is the `last` of them.
fn emit(pending: &mut Vec<u8>, last: bool, live: &mut dyn FnMut(String)) {
    while !pending.is_empty() {
        let take = pending.len().min(OUTPUT_CHUNK);
        let whole = last && take == pending.len();
        let cut = match std::str::from_utf8(pending.get(..take).unwrap_or_default()) {
            Err(e) if e.error_len().is_none() && !whole => e.valid_up_to(),
            _ => take,
        };
        if cut == 0 {
            return;
        }
        let piece: Vec<u8> = pending.drain(..cut).collect();
        live(String::from_utf8_lossy(&piece).into_owned());
    }
}

/// Performs one call, giving `sink` its process's output as it comes.
fn act(
    call: &Call,
    config: &Config,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(&[u8]),
) -> Result<Done, String> {
    match call {
        Call::Read {
            path,
            offset,
            limit,
        } => {
            let at = config.path(path, "path")?;
            let view = files::read(&at, *offset, *limit, cancel)?;
            Ok(Done {
                text: view.render(path),
                kept: None,
                digest: Some(view.digest),
                digests: Vec::new(),
            })
        }
        Call::Write {
            path,
            content,
            expected,
        } => {
            let at = config.path(path, "path")?;
            let written = files::write(&at, content, expected.as_deref())?;
            let verb = if written.created {
                "created"
            } else {
                "replaced"
            };
            Ok(Done {
                text: format!("{verb} {path} ({} bytes)", written.bytes),
                kept: None,
                digest: Some(written.digest),
                digests: Vec::new(),
            })
        }
        Call::Edit {
            path,
            old,
            new,
            all,
            expected,
        } => {
            let at = config.path(path, "path")?;
            let edited = files::edit(&at, old, new, *all, expected.as_deref())?;
            let places = if edited.replaced == 1 {
                "1 place".to_string()
            } else {
                format!("{} places", edited.replaced)
            };
            Ok(Done {
                text: format!("edited {path}: replaced {places}"),
                kept: None,
                digest: Some(edited.digest),
                digests: Vec::new(),
            })
        }
        Call::Patch { patch, expected } => {
            let parsed = crate::patch::parse(patch)?;
            let patched = files::patch(&parsed, expected, &|path| {
                config.path(path, "a patch's path")
            })?;
            Ok(Done {
                text: format!("applied the patch: {}", patched.said.join("; ")),
                kept: None,
                digest: None,
                digests: patched.digests,
            })
        }
        Call::Glob { pattern, path } => {
            let base = match path {
                Some(path) => config.path(path, "path")?,
                None => config.first("path")?,
            };
            let found = files::glob(pattern, &base)?;
            let mut text = if found.paths.is_empty() {
                format!("[no files under {} match {pattern}]\n", base.display())
            } else {
                found.paths.join("\n") + "\n"
            };
            if found.more {
                text.push_str(&format!(
                    "[stopped at {} paths or {} entries looked at; narrow the pattern or the path]\n",
                    files::MAX_GLOB,
                    files::MAX_GLOB_VISITS
                ));
            }
            Ok(Done {
                text,
                kept: None,
                digest: None,
                digests: Vec::new(),
            })
        }
        Call::Shell {
            command,
            timeout_ms,
            workdir,
        }
        | Call::ReviewShell {
            command,
            timeout_ms,
            workdir,
            ..
        } => {
            let timeout = shell::timeout(*timeout_ms)?;
            let dir = match workdir {
                Some(dir) => config.path(dir, "workdir")?,
                None => config.first("workdir")?,
            };
            if !dir.is_dir() {
                return Err(format!("`workdir` {} is not a directory", dir.display()));
            }
            let mut process = if let Call::ReviewShell {
                cargo_args: Some(args),
                ..
            } = call
            {
                let mut process = std::process::Command::new("cargo");
                process
                    .args(args)
                    .current_dir(&dir)
                    .env_clear()
                    .envs(shell::environment());
                process
            } else {
                shell::shell(command, &dir, config.proxy)
            };
            if let Call::ReviewShell {
                target_dir,
                runner,
                cargo_home,
                ..
            } = call
            {
                if config.proxy {
                    return Err("review commands cannot use a network proxy".into());
                }
                let target = config.path(target_dir, "target_dir")?;
                std::fs::create_dir_all(&target)
                    .map_err(|e| format!("{}: {e}", target.display()))?;
                process
                    .env("CARGO_TARGET_DIR", target)
                    .env("CARGO_NET_OFFLINE", REVIEW_CARGO_OFFLINE);
                if let Some(home) = cargo_home {
                    process.env("CARGO_HOME", config.path(home, "cargo_home")?);
                }
                if let Some(runner) = runner {
                    let runner = config.path(runner, "runner")?;
                    let value = format!("{} run-capped", runner.display());
                    for name in [
                        "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER",
                        "CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUNNER",
                    ] {
                        process.env(name, &value);
                    }
                }
            }
            let review = matches!(call, Call::ReviewShell { .. });
            let exit = if review {
                shell::run_whole(process, timeout, cancel, sink)?
            } else {
                shell::run(process, timeout, cancel, sink)?
            };
            let mut text = exit.render();
            if review && exit.cut {
                text.push_str(shell::CUT_NOTE);
            }
            Ok(Done {
                text,
                kept: Some(exit.output.text()),
                digest: None,
                digests: Vec::new(),
            })
        }
        // Its output went up as it came; the answer is how it ended.
        Call::Background {
            command,
            timeout_ms,
            workdir,
        } => {
            let timeout = shell::background_timeout(*timeout_ms)?;
            let dir = match workdir {
                Some(dir) => config.path(dir, "workdir")?,
                None => config.first("workdir")?,
            };
            if !dir.is_dir() {
                return Err(format!("`workdir` {} is not a directory", dir.display()));
            }
            let exit = shell::run_whole(
                shell::shell(command, &dir, config.proxy),
                timeout,
                cancel,
                sink,
            )?;
            let mut text = exit.status();
            if exit.cut {
                text.push_str(shell::CUT_NOTE);
            }
            Ok(Done {
                text,
                kept: None,
                digest: None,
                digests: Vec::new(),
            })
        }
        Call::Serve { .. } => Err("a serve call is run with its input".into()),
        Call::Grep {
            pattern,
            path,
            include,
            exclude,
            extended,
            ignore_case,
            context,
        } => {
            let txt = config.txt("grep")?;
            let base = match path {
                Some(path) => config.path(path, "path")?,
                None => config.first("path")?,
            };
            let grep = Grep {
                pattern: pattern.clone(),
                paths: vec![base.clone()],
                include: include.clone(),
                exclude: exclude.clone(),
                extended: *extended,
                ignore_case: *ignore_case,
                context: *context,
            };
            let workdir = if base.is_dir() {
                base.clone()
            } else {
                base.parent()
                    .map_or_else(|| base.clone(), Path::to_path_buf)
            };
            let exit = shell::run(
                grep.command(txt, &workdir),
                shell::DEFAULT_TIMEOUT,
                cancel,
                sink,
            )?;
            Ok(Done {
                text: Grep::render(&exit),
                kept: None,
                digest: None,
                digests: Vec::new(),
            })
        }
        Call::Sed {
            script,
            paths,
            extended,
        } => {
            let txt = config.txt("sed")?;
            let paths = paths
                .iter()
                .map(|p| config.path(p, "paths"))
                .collect::<Result<Vec<_>, _>>()?;
            let workdir = match (config.first("paths"), paths.first()) {
                (Ok(root), _) => root,
                (Err(_), Some(first)) => first
                    .parent()
                    .map_or_else(|| PathBuf::from("/"), Path::to_path_buf),
                (Err(_), None) => return Err("`paths` is empty; name the files to change".into()),
            };
            let sed = Sed {
                script: script.clone(),
                paths,
                extended: *extended,
            };
            let exit = shell::run(
                sed.command(txt, &workdir)?,
                shell::DEFAULT_TIMEOUT,
                cancel,
                sink,
            )?;
            let text = match (exit.stopped, exit.code) {
                (None, Some(0)) if exit.output.total() == 0 => {
                    format!("sed ran over {} files", sed.paths.len())
                }
                _ => exit.render(),
            };
            Ok(Done {
                text,
                kept: None,
                digest: None,
                digests: Vec::new(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use crate::host::Client;
    use std::time::{Duration, Instant};

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "td-agent-host-{tag}-{}-{}",
            std::process::id(),
            crate::store::random_hex(4).unwrap()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A tool host on a thread, served over a socketpair.
    fn host(roots: Vec<PathBuf>) -> Client {
        hosted(Config {
            roots,
            ..Config::default()
        })
    }

    fn hosted(config: Config) -> Client {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let reader = theirs.try_clone().unwrap();
        std::thread::spawn(move || serve(reader, theirs, config));
        Client::over(ours.try_clone().unwrap(), ours)
    }

    fn done(client: &mut Client, want: u64) -> Result<Done, String> {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Some(Ok(Up::Done { id, outcome })) = client.next_reply(Duration::from_millis(50))
            {
                if id == want {
                    return outcome;
                }
            }
        }
        panic!("call {want} did not end")
    }

    #[test]
    fn file_calls_keep_the_read_before_write_rule_across_the_protocol() {
        let dir = scratch("files");
        let mut client = host(vec![dir.clone()]);
        let path = dir.join("a.txt").display().to_string();
        let id = client
            .call(Call::Write {
                path: path.clone(),
                content: "one\n".into(),
                expected: None,
            })
            .unwrap();
        let made = done(&mut client, id).unwrap();
        assert!(made.text.starts_with("created"));
        let id = client
            .call(Call::Edit {
                path: path.clone(),
                old: "one".into(),
                new: "two".into(),
                all: false,
                expected: None,
            })
            .unwrap();
        assert!(done(&mut client, id)
            .unwrap_err()
            .contains("has not read it"));
        let id = client
            .call(Call::Edit {
                path: path.clone(),
                old: "one".into(),
                new: "two".into(),
                all: false,
                expected: made.digest,
            })
            .unwrap();
        assert!(done(&mut client, id)
            .unwrap()
            .text
            .contains("replaced 1 place"));
        let id = client
            .call(Call::Read {
                path: path.clone(),
                offset: None,
                limit: None,
            })
            .unwrap();
        let read = done(&mut client, id).unwrap();
        assert_eq!(read.text, "     1\ttwo\n");
        assert_eq!(read.digest.unwrap(), files::digest(b"two\n"));
        let id = client
            .call(Call::Glob {
                pattern: "*.txt".into(),
                path: None,
            })
            .unwrap();
        assert_eq!(done(&mut client, id).unwrap().text, format!("{path}\n"));
        let id = client
            .call(Call::Read {
                path: "a.txt".into(),
                offset: None,
                limit: None,
            })
            .unwrap();
        assert!(done(&mut client, id).unwrap_err().contains("relative"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A background call's output all comes up, however slowly it is
    /// taken, the end of it included: the host waits for room rather
    /// than dropping it, and its answer is how it ended (DESIGN.md §12).
    #[test]
    fn a_background_calls_output_waits_for_room_and_none_is_dropped() {
        let dir = scratch("background");
        let mut client = host(vec![dir.clone()]);
        // More than every queue on the way holds, taken a piece at a time
        // and slowly, so the end is still on its way when the process ends.
        let id = client
            .call(Call::Background {
                command: "i=0; while [ $i -lt 8000 ]; do printf '%01000d' 0; i=$((i+1)); done"
                    .into(),
                timeout_ms: None,
                workdir: None,
            })
            .unwrap();
        let mut bytes = 0;
        let deadline = Instant::now() + Duration::from_secs(120);
        let ended = loop {
            assert!(Instant::now() < deadline, "{bytes} bytes so far");
            match client.next_reply(Duration::from_millis(50)) {
                Some(Ok(Up::Output { id: of, text })) if of == id => {
                    bytes += text.len();
                    std::thread::sleep(Duration::from_millis(20));
                }
                Some(Ok(Up::Done { id: of, outcome })) if of == id => break outcome,
                _ => {}
            }
        };
        assert_eq!(bytes, 8_000_000);
        assert_eq!(ended.unwrap().text, "exit status 0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A served program's replies, standard error and end, read until it
    /// ends or `deadline`.
    fn served(client: &mut Client, id: u64, deadline: Instant) -> (Vec<u8>, String, Done) {
        let (mut output, mut errors) = (Vec::new(), String::new());
        loop {
            assert!(Instant::now() < deadline, "{} bytes so far", output.len());
            match client.next_reply(Duration::from_millis(50)) {
                Some(Ok(Up::RawOutput { id: of, bytes })) if of == id => {
                    output.extend_from_slice(&bytes)
                }
                Some(Ok(Up::Output { id: of, text })) if of == id => errors.push_str(&text),
                Some(Ok(Up::Done { id: of, outcome })) if of == id => {
                    return (output, errors, outcome.unwrap())
                }
                Some(Err(why)) => panic!("{why}"),
                _ => {}
            }
        }
    }

    /// A served program reads its input line by line as it comes, its
    /// output and standard error kept apart, and ends when its input
    /// does (DESIGN.md §12, MCP servers).
    #[test]
    fn a_served_program_reads_its_input_and_ends_with_it() {
        let dir = scratch("serve");
        let mut client = host(vec![dir.clone()]);
        let id = client
            .call(Call::Serve {
                program: "sh".into(),
                args: vec![
                    "-c".into(),
                    "while read l; do echo \"got:$l\"; echo \"err:$l\" >&2; done; echo bye".into(),
                ],
                workdir: None,
            })
            .unwrap();
        assert_eq!(client.input(id, b"one\n").unwrap(), 4);
        // An answer to the first line before the second is sent.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut output = Vec::new();
        while !output.ends_with(b"got:one\n") {
            assert!(Instant::now() < deadline);
            if let Some(Ok(Up::RawOutput { bytes, .. })) =
                client.next_reply(Duration::from_millis(50))
            {
                output.extend_from_slice(&bytes);
            }
        }
        assert_eq!(client.input(id, b"two\n").unwrap(), 4);
        client.close_input(id).unwrap();
        let (rest, errors, done) = served(&mut client, id, deadline);
        output.extend_from_slice(&rest);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "got:one\ngot:two\nbye\n"
        );
        assert!(errors.ends_with("err:two\n"), "{errors}");
        assert_eq!(done.text, "exit status 0");
        // Input to a call that ended is refused.
        assert!(client.input(id, b"x").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// More input than one window goes through as the program takes it,
    /// every byte and in order.
    #[test]
    fn a_served_programs_input_flows_past_its_window() {
        let dir = scratch("serve-window");
        let mut client = host(vec![dir.clone()]);
        let id = client
            .call(Call::Serve {
                program: "wc".into(),
                args: vec!["-c".into()],
                workdir: None,
            })
            .unwrap();
        let data = vec![b'x'; 1024 * 1024];
        let mut sent = 0;
        let deadline = Instant::now() + Duration::from_secs(60);
        while sent < data.len() {
            assert!(Instant::now() < deadline, "{sent} bytes sent");
            let went = client.input(id, data.get(sent..).unwrap()).unwrap();
            if went == 0 {
                std::thread::sleep(Duration::from_millis(5));
            }
            sent += went;
        }
        client.close_input(id).unwrap();
        let (output, _, done) = served(&mut client, id, deadline);
        assert_eq!(String::from_utf8(output).unwrap().trim(), "1048576");
        assert_eq!(done.text, "exit status 0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Input to a program that closed its input is taken all the same,
    /// so the sender is never held at its window by a pipe nobody reads.
    #[test]
    fn input_past_a_closed_input_is_still_taken() {
        let dir = scratch("serve-closed");
        let mut client = host(vec![dir.clone()]);
        let id = client
            .call(Call::Serve {
                program: "sh".into(),
                args: vec!["-c".into(), "exec 0<&-; sleep 6; echo ended".into()],
                workdir: None,
            })
            .unwrap();
        let data = vec![b'x'; 3 * crate::host::INPUT_WINDOW as usize];
        let mut went = 0;
        let deadline = Instant::now() + Duration::from_secs(20);
        while went < data.len() {
            assert!(Instant::now() < deadline, "{went} bytes went");
            went += client.input(id, data.get(went..).unwrap()).unwrap();
            if let Some(Ok(Up::Done { .. })) = client.next_reply(Duration::from_millis(20)) {
                panic!("it ended with {went} bytes sent");
            }
        }
        client.close_input(id).unwrap();
        let (output, _, done) = served(&mut client, id, Instant::now() + Duration::from_secs(20));
        assert_eq!(output, b"ended\n");
        assert_eq!(done.text, "exit status 0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A program that never reads its input holds up nothing else: its
    /// window fills and no more is sent, another call is answered, and a
    /// cancel ends it at once.
    #[test]
    fn a_program_that_reads_nothing_holds_up_nothing() {
        let dir = scratch("serve-stuck");
        let mut client = host(vec![dir.clone()]);
        let stuck = client
            .call(Call::Serve {
                program: "sleep".into(),
                args: vec!["60".into()],
                workdir: None,
            })
            .unwrap();
        // What the pipe holds before the program would have to read is
        // taken, so a little more than one window goes; then nothing.
        let window = crate::host::INPUT_WINDOW as usize;
        let data = vec![b'x'; 4 * window];
        let mut went = 0;
        let mut idle = 0;
        while idle < 5 {
            let sent = client.input(stuck, &data).unwrap();
            went += sent;
            idle = if sent == 0 { idle + 1 } else { 0 };
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(
            (window..=window + 128 * 1024).contains(&went),
            "{went} bytes went"
        );
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(client.input(stuck, &data).unwrap(), 0);
        let quick = client
            .call(Call::Shell {
                command: "echo here".into(),
                timeout_ms: None,
                workdir: None,
            })
            .unwrap();
        let started = Instant::now();
        assert!(done(&mut client, quick).unwrap().text.contains("here"));
        client.cancel(stuck).unwrap();
        let ended = done(&mut client, stuck).unwrap();
        assert!(ended.text.starts_with("interrupted"), "{}", ended.text);
        assert!(started.elapsed() < Duration::from_secs(20));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn review_output_keeps_raw_bytes_with_a_slow_receiver() {
        let dir = scratch("review-output");
        let mut client = hosted(Config {
            roots: vec![dir.clone()],
            proxy: false,
            ..Default::default()
        });
        let expected = 16 * 1024 * 1024;
        let id = client
            .call(Call::ReviewShell {
                cargo_args: None,
                command: format!("head -c {expected} /dev/zero; printf '\\377'"),
                timeout_ms: Some(30_000),
                workdir: None,
                target_dir: dir.join("target").display().to_string(),
                runner: None,
                cargo_home: None,
            })
            .unwrap();
        std::thread::sleep(Duration::from_secs(1));
        let mut output = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(40);
        let ended = loop {
            assert!(Instant::now() < deadline, "{} bytes so far", output.len());
            match client.next_reply(Duration::from_millis(50)) {
                Some(Ok(Up::RawOutput { id: got, bytes })) if got == id => {
                    output.extend_from_slice(&bytes);
                    std::thread::sleep(Duration::from_millis(2));
                }
                Some(Ok(Up::Done { id: got, outcome })) if got == id => break outcome,
                Some(Err(why)) => panic!("{why}"),
                _ => {}
            }
        };
        assert!(ended.unwrap().text.starts_with("[exit status 0]"));
        assert_eq!(output.len(), expected + 1);
        assert_eq!(output.last(), Some(&255));
        assert!(output
            .get(..expected)
            .unwrap()
            .iter()
            .all(|byte| *byte == 0));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn review_output_drains_a_descendant_that_keeps_writing() {
        let dir = scratch("review-drain");
        let config = Config {
            roots: vec![dir.clone()],
            proxy: false,
            ..Default::default()
        };
        let mut output = Vec::new();
        let done = act(&Call::ReviewShell {
            cargo_args: None,
            command: "(i=0; while [ $i -lt 8 ]; do printf .; sleep 0.1; i=$((i+1)); done; printf TRAILING) & printf EARLY".into(),
            timeout_ms: Some(5_000), workdir: None,
            target_dir: dir.join("target").display().to_string(),
            runner: None, cargo_home: None,
        }, &config, &AtomicBool::new(false), &mut |bytes| output.extend_from_slice(bytes)).unwrap();
        assert!(String::from_utf8(output).unwrap().contains("TRAILING"));
        assert!(done.text.contains("TRAILING"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn shell_calls_run_side_by_side_stream_and_cancel() {
        let dir = scratch("shell");
        let mut client = host(vec![dir.clone()]);
        let slow = client
            .call(Call::Shell {
                command: "echo slow; exec sleep 30".into(),
                timeout_ms: None,
                workdir: None,
            })
            .unwrap();
        let quick = client
            .call(Call::Shell {
                command: "pwd".into(),
                timeout_ms: None,
                workdir: None,
            })
            .unwrap();
        // The quick one ends while the slow one runs.
        let mut streamed = String::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut quick_done = None;
        while quick_done.is_none() || streamed.is_empty() {
            assert!(Instant::now() < deadline);
            match client.next_reply(Duration::from_millis(50)) {
                Some(Ok(Up::Output { id, text })) if id == slow => streamed.push_str(&text),
                Some(Ok(Up::Done { id, outcome })) if id == quick => quick_done = Some(outcome),
                _ => {}
            }
        }
        assert_eq!(
            quick_done.unwrap().unwrap().text,
            format!("[exit status 0]\n{}\n", dir.display())
        );
        assert_eq!(streamed, "slow\n");
        client.cancel(slow).unwrap();
        let ended = done(&mut client, slow).unwrap();
        assert!(ended.text.starts_with("[interrupted"), "{}", ended.text);
        assert_eq!(ended.kept.as_deref(), Some("slow\n"));
        assert_eq!(client.in_flight(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_cannot_run_says_why() {
        let mut client = host(Vec::new());
        let id = client
            .call(Call::Shell {
                command: "true".into(),
                timeout_ms: None,
                workdir: None,
            })
            .unwrap();
        assert!(done(&mut client, id).unwrap_err().contains("no worktree"));
        let id = client
            .call(Call::Grep {
                pattern: "x".into(),
                path: Some("/".into()),
                include: None,
                exclude: None,
                extended: false,
                ignore_case: false,
                context: None,
            })
            .unwrap();
        assert!(done(&mut client, id).unwrap_err().contains("no td-txt"));
        let id = client
            .call(Call::Shell {
                command: "true".into(),
                timeout_ms: Some(0),
                workdir: Some("/".into()),
            })
            .unwrap();
        assert!(done(&mut client, id).unwrap_err().contains("timeout_ms"));
    }

    #[test]
    fn calls_past_the_bound_are_refused() {
        let dir = scratch("bound");
        let mut client = host(vec![dir.clone()]);
        let ids: Vec<u64> = (0..MAX_CALLS)
            .map(|_| {
                client
                    .call(Call::Shell {
                        command: "exec sleep 30".into(),
                        timeout_ms: None,
                        workdir: None,
                    })
                    .unwrap()
            })
            .collect();
        // Give the host time to take them all.
        std::thread::sleep(Duration::from_millis(300));
        let over = client
            .call(Call::Glob {
                pattern: "*".into(),
                path: None,
            })
            .unwrap();
        assert!(done(&mut client, over)
            .unwrap_err()
            .contains("already running"));
        for id in ids {
            client.cancel(id).unwrap();
            assert!(done(&mut client, id)
                .unwrap()
                .text
                .starts_with("[interrupted"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_output_is_cut_between_characters() {
        let mut pieces = Vec::new();
        let mut pending = "\u{e9}".as_bytes()[..1].to_vec();
        emit(&mut pending, false, &mut |p| pieces.push(p));
        assert!(pieces.is_empty(), "half a character waits");
        pending.extend_from_slice(&"\u{e9}x".as_bytes()[1..]);
        emit(&mut pending, false, &mut |p| pieces.push(p));
        assert_eq!(pieces, ["\u{e9}x"]);
        let mut pending = "\u{e9}".repeat(OUTPUT_CHUNK).into_bytes();
        let mut pieces = Vec::new();
        emit(&mut pending, false, &mut |p| pieces.push(p));
        assert!(pieces.iter().all(|p| !p.contains('\u{fffd}')));
        assert_eq!(pieces.concat(), "\u{e9}".repeat(OUTPUT_CHUNK));
        let mut pending = vec![0xc3];
        let mut pieces = Vec::new();
        emit(&mut pending, true, &mut |p| pieces.push(p));
        assert_eq!(pieces, ["\u{fffd}"], "the last bytes go whatever they are");
    }

    #[test]
    fn a_wrong_call_is_its_own_error_and_a_huge_result_one_too() {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let reader = theirs.try_clone().unwrap();
        std::thread::spawn(move || serve(reader, theirs, Config::default()));
        let mut conversation = ours;
        frame::write(
            &mut conversation,
            br#"{"call":7,"tool":"read_file","args":{}}"#,
        )
        .unwrap();
        let up = Up::decode(&frame::read(&mut conversation).unwrap().unwrap()).unwrap();
        let Up::Done {
            id: 7,
            outcome: Err(why),
        } = up
        else {
            panic!("{up:?}")
        };
        assert!(why.contains("no `path`"), "{why}");
        // The host is still serving.
        frame::write(
            &mut conversation,
            br#"{"call":8,"tool":"glob","args":{"pattern":"*"}}"#,
        )
        .unwrap();
        let up = Up::decode(&frame::read(&mut conversation).unwrap().unwrap()).unwrap();
        assert!(
            matches!(
                up,
                Up::Done {
                    id: 8,
                    outcome: Err(_)
                }
            ),
            "{up:?}"
        );
        let (mut ours, mut theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let huge = Up::Done {
            id: 3,
            outcome: Ok(Done {
                text: "\u{1}".repeat(frame::MAX_FRAME),
                kept: None,
                digest: None,
                digests: Vec::new(),
            }),
        };
        reply(&mut ours, &huge);
        let up = Up::decode(&frame::read(&mut theirs).unwrap().unwrap()).unwrap();
        let Up::Done {
            id: 3,
            outcome: Err(why),
        } = up
        else {
            panic!("{up:?}")
        };
        assert!(why.contains("past the 1048576-byte frame"), "{why}");
    }

    /// A working directory not yet bound refuses a call that names no
    /// directory, rather than another root taking its place.
    #[test]
    fn a_working_directory_not_bound_refuses_and_names_none_in_its_place() {
        let other = std::env::temp_dir();
        let mut client = hosted(Config {
            roots: vec![other.clone()],
            txt: None,
            directory: Some("/w/not-yet".into()),
            proxy: false,
        });
        let id = client
            .call(Call::Shell {
                command: "true".into(),
                timeout_ms: None,
                workdir: None,
            })
            .unwrap();
        let e = done(&mut client, id).unwrap_err();
        assert!(e.contains("/w/not-yet, is not checked out yet"), "{e}");
        let id = client
            .call(Call::Shell {
                command: "true".into(),
                timeout_ms: None,
                workdir: Some(other.display().to_string()),
            })
            .unwrap();
        assert!(done(&mut client, id).is_ok());
        let mut client = hosted(Config {
            roots: vec![other.clone(), "/w/ready".into()],
            txt: None,
            directory: Some("/w/ready".into()),
            proxy: false,
        });
        let config = Config::parse(&["--directory".into(), "/w/ready".into()]).unwrap();
        assert_eq!(config.directory, Some(PathBuf::from("/w/ready")));
        assert!(Config::parse(&["--directory".into(), "w".into()]).is_err());
        let id = client
            .call(Call::Shell {
                command: "true".into(),
                timeout_ms: None,
                workdir: None,
            })
            .unwrap();
        // The bound working directory is chosen: it does not exist here.
        let e = done(&mut client, id).unwrap_err();
        assert!(e.contains("/w/ready"), "{e}");
    }

    #[test]
    fn the_configuration_is_absolute_paths() {
        let config = Config::parse(&[
            "--root".into(),
            "/w/a".into(),
            "--txt".into(),
            "/bin/td-txt".into(),
            "--root".into(),
            "/w/b".into(),
        ])
        .unwrap();
        assert_eq!(config.roots, [PathBuf::from("/w/a"), PathBuf::from("/w/b")]);
        assert_eq!(config.txt, Some(PathBuf::from("/bin/td-txt")));
        assert!(Config::parse(&["--root".into(), "w".into()]).is_err());
        assert!(Config::parse(&["--root".into()]).is_err());
        assert!(Config::parse(&["--x".into(), "/a".into()]).is_err());
    }
}
