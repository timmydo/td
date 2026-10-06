//! The process tools' semantics (DESIGN.md §12): `shell` runs one
//! `sh -c` per call, and `grep` and `sed` run td-txt's applets with their
//! structured arguments mapped to its options. The tool host runs each in
//! a jail instance of its own (§8), whose teardown ends every process the
//! call started; here a call ends when its process does, or is killed at
//! its timeout or when it is cancelled, and output a leftover descendant
//! writes after that is not waited for.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// A `shell` call's timeout when it names none, and the longest it may
/// name.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
pub const MAX_TIMEOUT: Duration = Duration::from_secs(600);
/// The longest a background process runs, and how long it runs when its
/// call names no timeout (DESIGN.md §12).
pub const MAX_BACKGROUND_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
/// What is kept of a call's output, standard error interleaved: its first
/// and its last this many bytes, the rest counted (the log's record).
pub const KEPT_HEAD: usize = 64 * 1024;
pub const KEPT_TAIL: usize = 64 * 1024;
/// What the model is shown of it: its first and last this many bytes.
pub const SHOWN_HEAD: usize = 15 * 1024;
pub const SHOWN_TAIL: usize = 15 * 1024;
/// The most lines of matches `grep` shows.
pub const MAX_GREP_LINES: usize = 1000;
/// How long output is still read after the call's process ended, for a
/// descendant's last writes, before the call returns without it.
const DRAIN: Duration = Duration::from_millis(300);
/// How often the wait looks at the process, the clock and the cancel.
const TICK: Duration = Duration::from_millis(20);
/// The most pieces of output read ahead of the call taking them.
const PIPE_QUEUE: usize = 16;

/// Output kept as it streams: its head and tail, and how much there was.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Kept {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: u64,
    head_cap: usize,
    tail_cap: usize,
}

impl Kept {
    pub fn new(head: usize, tail: usize) -> Self {
        Self {
            head_cap: head,
            tail_cap: tail,
            ..Self::default()
        }
    }

    /// Takes the next bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len() as u64);
        let room = self.head_cap.saturating_sub(self.head.len());
        let (to_head, rest) = bytes.split_at(room.min(bytes.len()));
        self.head.extend_from_slice(to_head);
        self.tail.extend(rest);
        let over = self.tail.len().saturating_sub(self.tail_cap);
        self.tail.drain(..over);
    }

    /// Every byte there was.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// The bytes left out between head and tail.
    pub fn omitted(&self) -> u64 {
        self.total
            .saturating_sub(self.head.len() as u64)
            .saturating_sub(self.tail.len() as u64)
    }

    /// The kept bytes as text, a line naming how many were left out
    /// between head and tail.
    pub fn text(&self) -> String {
        let mut tail: Vec<u8> = self.tail.iter().copied().collect();
        match self.omitted() {
            0 => {
                let mut all = self.head.clone();
                all.append(&mut tail);
                String::from_utf8_lossy(&all).into_owned()
            }
            n => {
                // A character the cut split is counted as omitted, not
                // shown as two replacement characters.
                let head_end = whole_end(&self.head);
                let tail_start = tail
                    .iter()
                    .take(3)
                    .take_while(|b| **b & 0xC0 == 0x80)
                    .count();
                let n = n
                    .saturating_add((self.head.len() - head_end) as u64)
                    .saturating_add(tail_start as u64);
                let head = String::from_utf8_lossy(self.head.get(..head_end).unwrap_or_default());
                let tail = String::from_utf8_lossy(tail.get(tail_start..).unwrap_or_default());
                format!("{head}\n[... {n} bytes omitted ...]\n{tail}")
            }
        }
    }

    /// The same output kept smaller, as the model is shown it.
    pub fn shown(&self, head: usize, tail: usize) -> Self {
        let mut smaller = Self::new(head, tail);
        smaller.push(&self.head);
        let gap = self.omitted();
        let tail_bytes: Vec<u8> = self.tail.iter().copied().collect();
        // The gap between the kept parts is counted as if it had passed.
        smaller.total = smaller.total.saturating_add(gap);
        if gap > 0 {
            let keep = smaller.tail_cap;
            smaller.tail.clear();
            let from = tail_bytes.len().saturating_sub(keep);
            smaller
                .tail
                .extend(tail_bytes.get(from..).unwrap_or_default());
            smaller.total = smaller.total.saturating_add(tail_bytes.len() as u64);
        } else {
            smaller.push(&tail_bytes);
        }
        smaller
    }
}

/// Where `bytes` ends in whole characters: before a last character it
/// holds only the start of.
fn whole_end(bytes: &[u8]) -> usize {
    let len = bytes.len();
    for back in 1..=len.min(4) {
        let at = len - back;
        let Some(&b) = bytes.get(at) else {
            break;
        };
        if b & 0xC0 == 0x80 {
            continue;
        }
        let needed = match b {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        return if back < needed { at } else { len };
    }
    len
}

/// Why a call's process was stopped before it ended by itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stopped {
    TimedOut,
    Cancelled,
}

/// How a call's process ended, and its output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Exit {
    /// Its exit status, when it exited.
    pub code: Option<i32>,
    /// The signal that ended it, when one did.
    pub signal: Option<i32>,
    pub stopped: Option<Stopped>,
    pub output: Kept,
    pub elapsed: Duration,
    /// The timeout the call ran under.
    pub timeout: Duration,
    /// Output still came at `MAX_LINGER` after its end, and the rest was
    /// not read.
    pub cut: bool,
}

impl Exit {
    /// The line that says how it ended.
    pub fn status(&self) -> String {
        let how = match (self.code, self.signal) {
            (Some(code), _) => format!("exit status {code}"),
            (None, Some(signal)) => format!("killed by signal {signal}"),
            (None, None) => "no exit status".to_string(),
        };
        match self.stopped {
            Some(Stopped::TimedOut) => {
                format!("timed out after {} ms, {how}", self.timeout.as_millis())
            }
            Some(Stopped::Cancelled) => format!("interrupted, {how}"),
            None => how,
        }
    }

    /// The result as the model is given it: the status, then the output
    /// cut to its head and tail.
    pub fn render(&self) -> String {
        let shown = self.output.shown(SHOWN_HEAD, SHOWN_TAIL);
        let mut out = format!("[{}]\n", self.status());
        out.push_str(&shown.text());
        out
    }
}

/// The environment a call's process gets: `PATH`, `HOME`, `TMPDIR` and
/// `LANG` as the tool host has them, and `TERM=dumb` (§8).
pub fn environment() -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = ["PATH", "HOME", "TMPDIR", "LANG"]
        .into_iter()
        .filter_map(|name| Some((OsString::from(name), std::env::var_os(name)?)))
        .collect();
    env.push(("TERM".into(), "dumb".into()));
    env
}

/// Runs `command`, its standard input empty and its standard output and
/// error one stream, until it ends, `timeout` passes or `cancel` is set,
/// giving `sink` each piece of output as it comes; after it ends, what
/// is left is read for `DRAIN`.
pub fn run(
    command: Command,
    timeout: Duration,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(&[u8]),
) -> Result<Exit, String> {
    run_draining(command, timeout, cancel, sink, false)
}

/// `run` for a background process, whose output is all kept: after it
/// ends, what is left is read until the pipe is quiet for `DRAIN`,
/// however long `sink` takes each piece, for at most `MAX_LINGER`.
pub fn run_whole(
    command: Command,
    timeout: Duration,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(&[u8]),
) -> Result<Exit, String> {
    run_draining(command, timeout, cancel, sink, true)
}

/// How long a background process's output is still read after it ends,
/// while a descendant still writes.
const MAX_LINGER: Duration = Duration::from_secs(60);

fn run_draining(
    mut command: Command,
    timeout: Duration,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(&[u8]),
    whole: bool,
) -> Result<Exit, String> {
    let started = Instant::now();
    let (mut reader, writer) = std::io::pipe().map_err(|e| format!("a pipe: {e}"))?;
    let errors = writer.try_clone().map_err(|e| format!("a pipe: {e}"))?;
    command.stdin(Stdio::null()).stdout(writer).stderr(errors);
    let mut child = command
        .spawn()
        .map_err(|e| format!("{}: {e}", command.get_program().to_string_lossy()))?;
    // The command holds the pipe's writing end; the read ends only when
    // every holder has closed it.
    drop(command);
    // Bounded, so output the call has not taken waits in the pipe, and
    // the writer with it, rather than in this process's memory.
    let (chunks, from_pipe) = mpsc::sync_channel::<Vec<u8>>(PIPE_QUEUE);
    let reading = std::thread::Builder::new().spawn(move || {
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if chunks
                        .send(buffer.get(..n).unwrap_or_default().to_vec())
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    });
    if let Err(e) = reading {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("a thread to read the output: {e}"));
    }
    let deadline = started + timeout;
    let mut output = Kept::new(KEPT_HEAD, KEPT_TAIL);
    let mut stopped = None;
    let mut status = None;
    let mut ended_at: Option<Instant> = None;
    // When the pipe last brought output.
    let mut heard = started;
    // Whether the pipe may still bring output: a process that closes its
    // output and runs on is waited for by the clock, not by spinning.
    let mut open = true;
    let cut = loop {
        // The process's state, the cancel and the clock are looked at on
        // every pass, so a process that writes without pause still ends.
        let quiet_since = |at: Instant| if whole { at.max(heard) } else { at };
        if let Some(at) = ended_at {
            let lingered = whole && at.elapsed() >= MAX_LINGER;
            if !open || quiet_since(at).elapsed() >= DRAIN || lingered {
                break open && lingered;
            }
        } else if let Some(exited) = match child.try_wait() {
            Ok(exited) => exited,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("waiting: {e}"));
            }
        } {
            status = Some(exited);
            ended_at = Some(Instant::now());
        } else if let Some(why) = if cancel.load(Ordering::Relaxed) {
            Some(Stopped::Cancelled)
        } else if Instant::now() >= deadline {
            Some(Stopped::TimedOut)
        } else {
            None
        } {
            stopped = Some(why);
            let _ = child.kill();
            status = Some(child.wait().map_err(|e| format!("waiting: {e}"))?);
            ended_at = Some(Instant::now());
        }
        let wait = ended_at.map_or(TICK, |at| {
            DRAIN.saturating_sub(quiet_since(at).elapsed()).min(TICK)
        });
        if !open {
            std::thread::sleep(wait);
            continue;
        }
        match from_pipe.recv_timeout(wait) {
            Ok(chunk) => {
                output.push(&chunk);
                sink(&chunk);
                heard = Instant::now();
            }
            Err(RecvTimeoutError::Disconnected) => open = false,
            Err(RecvTimeoutError::Timeout) => {}
        }
    };
    let status = status.ok_or("the process's status was not read")?;
    Ok(Exit {
        code: status.code(),
        signal: status.signal(),
        stopped,
        output,
        elapsed: started.elapsed(),
        timeout,
        cut,
    })
}

/// A `shell` call: `sh -c command` in `workdir`.
pub fn shell(command: &str, workdir: &Path) -> Command {
    let mut sh = Command::new("sh");
    sh.arg("-c")
        .arg(command)
        .current_dir(workdir)
        .env_clear()
        .envs(environment());
    sh
}

/// A `shell` call's timeout: the default when none is given, refused past
/// the most.
pub fn timeout(ms: Option<u64>) -> Result<Duration, String> {
    let Some(ms) = ms else {
        return Ok(DEFAULT_TIMEOUT);
    };
    let asked = Duration::from_millis(ms);
    if ms == 0 || asked > MAX_TIMEOUT {
        return Err(format!(
            "`timeout_ms` is {ms}; it is from 1 to {}",
            MAX_TIMEOUT.as_millis()
        ));
    }
    Ok(asked)
}

/// A background process's timeout: the longest when none is given,
/// refused past it.
pub fn background_timeout(ms: Option<u64>) -> Result<Duration, String> {
    let Some(ms) = ms else {
        return Ok(MAX_BACKGROUND_TIMEOUT);
    };
    let asked = Duration::from_millis(ms);
    if ms == 0 || asked > MAX_BACKGROUND_TIMEOUT {
        return Err(format!(
            "`timeout_ms` is {ms}; in the background it is from 1 to {}",
            MAX_BACKGROUND_TIMEOUT.as_millis()
        ));
    }
    Ok(asked)
}

/// A `grep` call's arguments (§12).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Grep {
    pub pattern: String,
    pub paths: Vec<PathBuf>,
    pub include: Option<String>,
    pub exclude: Option<String>,
    pub extended: bool,
    pub ignore_case: bool,
    pub context: Option<u32>,
}

/// The most lines of context `grep` takes either side of a match.
pub const MAX_CONTEXT: u32 = 20;

impl Grep {
    /// td-txt's `grep -rnH`, naming the file even when `path` is one,
    /// with the arguments mapped to its options, the
    /// pattern after `-e` and the paths after `--`, so neither is read as
    /// an option.
    pub fn argv(&self) -> Vec<OsString> {
        let mut argv: Vec<OsString> = vec!["-r".into(), "-n".into(), "-H".into()];
        if self.extended {
            argv.push("-E".into());
        }
        if self.ignore_case {
            argv.push("-i".into());
        }
        if let Some(lines) = self.context {
            argv.push(format!("-C{}", lines.min(MAX_CONTEXT)).into());
        }
        if let Some(include) = &self.include {
            argv.push(format!("--include={include}").into());
        }
        if let Some(exclude) = &self.exclude {
            argv.push(format!("--exclude={exclude}").into());
        }
        argv.push("-e".into());
        argv.push(self.pattern.clone().into());
        argv.push("--".into());
        argv.extend(self.paths.iter().map(|p| p.clone().into_os_string()));
        argv
    }

    /// td-txt run as `grep`, which it dispatches on.
    pub fn command(&self, txt: &Path, workdir: &Path) -> Command {
        applet(txt, "grep", self.argv(), workdir)
    }

    /// The result as the model is given it: grep's exit status 1 is no
    /// match, not a failure; matches past `MAX_GREP_LINES` are counted.
    pub fn render(exit: &Exit) -> String {
        let text = exit.output.text();
        let mut lines = text.lines();
        let shown: Vec<&str> = lines.by_ref().take(MAX_GREP_LINES).collect();
        let more = lines.count();
        let mut out = match (exit.stopped, exit.code) {
            (None, Some(1)) if text.is_empty() => return "[no matches]\n".to_string(),
            (None, Some(0 | 1)) => String::new(),
            _ => format!("[{}]\n", exit.status()),
        };
        for line in shown {
            out.push_str(line);
            out.push('\n');
        }
        if more > 0 || exit.output.omitted() > 0 {
            out.push_str(&format!(
                "[{more} more lines and {} bytes not shown; narrow the pattern or the path]\n",
                exit.output.omitted()
            ));
        }
        out
    }
}

/// A `sed` call's arguments (§12).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Sed {
    pub script: String,
    pub paths: Vec<PathBuf>,
    pub extended: bool,
}

impl Sed {
    /// td-txt's `sed --sandbox -i` over the named files: the script after
    /// `-e` and the paths after `--`. `--sandbox` refuses `r`, `R`, `w`,
    /// `W` and the `w` flag, and td-txt's sed has no command that starts a
    /// process, so it reads and writes only the named files.
    pub fn argv(&self) -> Result<Vec<OsString>, String> {
        if self.paths.is_empty() {
            return Err("`paths` is empty; name the files to change".into());
        }
        let mut argv: Vec<OsString> = vec!["--sandbox".into(), "-i".into()];
        if self.extended {
            argv.push("-E".into());
        }
        argv.push("-e".into());
        argv.push(self.script.clone().into());
        argv.push("--".into());
        argv.extend(self.paths.iter().map(|p| p.clone().into_os_string()));
        Ok(argv)
    }

    /// td-txt run as `sed`.
    pub fn command(&self, txt: &Path, workdir: &Path) -> Result<Command, String> {
        Ok(applet(txt, "sed", self.argv()?, workdir))
    }
}

/// td-txt run as `name`, which it dispatches on, with the call's
/// environment.
fn applet(txt: &Path, name: &str, argv: Vec<OsString>, workdir: &Path) -> Command {
    let mut command = Command::new(txt);
    command
        .arg0(name)
        .args(argv)
        .current_dir(workdir)
        .env_clear()
        .envs(environment());
    command
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;

    fn quick(command: &str) -> Exit {
        let never = AtomicBool::new(false);
        run(
            shell(command, &std::env::temp_dir()),
            Duration::from_secs(10),
            &never,
            &mut |_| {},
        )
        .unwrap()
    }

    #[test]
    fn a_command_gives_its_status_and_interleaved_output() {
        let exit = quick("echo out; echo err >&2; exit 3");
        assert_eq!(exit.code, Some(3));
        assert_eq!(exit.output.text(), "out\nerr\n");
        assert_eq!(exit.render(), "[exit status 3]\nout\nerr\n");
        assert_eq!(quick("true").status(), "exit status 0");
        // Standard input is empty, and the environment is the scrubbed one.
        assert_eq!(quick("cat; echo done").output.text(), "done\n");
        assert_eq!(quick("echo $TERM").output.text(), "dumb\n");
    }

    #[test]
    fn a_timeout_or_a_cancel_kills_it() {
        let never = AtomicBool::new(false);
        let mut seen = Vec::new();
        let exit = run(
            shell("echo started; exec sleep 30", &std::env::temp_dir()),
            Duration::from_millis(300),
            &never,
            &mut |chunk| seen.extend_from_slice(chunk),
        )
        .unwrap();
        assert_eq!(exit.stopped, Some(Stopped::TimedOut));
        assert_eq!(exit.signal, Some(9));
        assert!(exit.elapsed < Duration::from_secs(5));
        assert_eq!(seen, b"started\n", "output streams as it comes");
        assert!(exit.status().starts_with("timed out after"));
        let cancelled = AtomicBool::new(true);
        let exit = run(
            shell("exec sleep 30", &std::env::temp_dir()),
            Duration::from_secs(30),
            &cancelled,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(exit.stopped, Some(Stopped::Cancelled));
        assert!(exit.status().starts_with("interrupted"));
    }

    #[test]
    fn a_descendant_holding_the_output_does_not_hold_the_call() {
        let started = Instant::now();
        let exit = quick("sleep 5 & echo parent");
        assert_eq!(exit.code, Some(0));
        assert!(exit.output.text().starts_with("parent"));
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn a_process_writing_without_pause_still_times_out_and_cancels() {
        let never = AtomicBool::new(false);
        let started = Instant::now();
        let exit = run(
            shell("while :; do echo y; done", &std::env::temp_dir()),
            Duration::from_millis(300),
            &never,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(exit.stopped, Some(Stopped::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(5));
        let cancelled = AtomicBool::new(true);
        let exit = run(
            shell("while :; do echo y; done", &std::env::temp_dir()),
            Duration::from_secs(30),
            &cancelled,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(exit.stopped, Some(Stopped::Cancelled));
        // A descendant writing after its parent ended holds the call no
        // longer than the drain.
        let started = Instant::now();
        let exit = quick("(i=0; while [ $i -lt 100000 ]; do echo y; i=$((i+1)); done) & exit 5");
        assert_eq!(exit.code, Some(5));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn a_process_that_closes_its_output_is_still_waited_for() {
        let exit = quick("exec >&- 2>&-; sleep 1; exit 4");
        assert_eq!(exit.code, Some(4));
        assert!(exit.elapsed >= Duration::from_secs(1));
        assert_eq!(exit.output.total(), 0);
    }

    #[test]
    fn output_is_kept_and_shown_as_head_and_tail() {
        let mut kept = Kept::new(4, 4);
        kept.push(b"abcdefgh");
        kept.push(b"ijklmnop");
        assert_eq!(kept.total(), 16);
        assert_eq!(kept.omitted(), 8);
        assert_eq!(kept.text(), "abcd\n[... 8 bytes omitted ...]\nmnop");
        let small = kept.shown(2, 2);
        assert_eq!(small.omitted(), 12);
        assert_eq!(small.text(), "ab\n[... 12 bytes omitted ...]\nop");
        // A character across the cut is kept whole or counted as omitted.
        let mut split = Kept::new(4, 4);
        split.push("abc\u{e9}".as_bytes());
        assert_eq!(split.text(), "abc\u{e9}");
        let mut split = Kept::new(4, 4);
        split.push("abc\u{e9}xyz\u{e9}12".as_bytes());
        assert_eq!(split.text(), "abc\n[... 5 bytes omitted ...]\n\u{e9}12");
        let mut whole = Kept::new(4, 4);
        whole.push(b"abcdef");
        assert_eq!(whole.text(), "abcdef");
        assert_eq!(
            whole.shown(2, 2).text(),
            "ab\n[... 2 bytes omitted ...]\nef"
        );
        let exit = quick("head -c 100000 /dev/zero | tr '\\0' x");
        assert_eq!(exit.output.total(), 100_000);
        let rendered = exit.render();
        assert!(rendered.contains(&format!(
            "[... {} bytes omitted ...]",
            100_000 - SHOWN_HEAD - SHOWN_TAIL
        )));
    }

    #[test]
    fn timeouts_are_defaulted_and_bounded() {
        assert_eq!(timeout(None).unwrap(), DEFAULT_TIMEOUT);
        assert_eq!(timeout(Some(5000)).unwrap(), Duration::from_secs(5));
        assert!(timeout(Some(0)).unwrap_err().contains("from 1 to 600000"));
        assert!(timeout(Some(600_001)).is_err());
        // In the background, a day, and a day at most.
        assert_eq!(background_timeout(None).unwrap(), MAX_BACKGROUND_TIMEOUT);
        assert_eq!(
            background_timeout(Some(86_400_000)).unwrap(),
            MAX_BACKGROUND_TIMEOUT
        );
        assert!(background_timeout(Some(0)).is_err());
        assert!(background_timeout(Some(86_400_001))
            .unwrap_err()
            .contains("in the background"));
    }

    #[test]
    fn grep_and_sed_map_onto_td_txt_options() {
        let grep = Grep {
            pattern: "-v".into(),
            paths: vec!["/w/a".into(), "-x".into()],
            include: Some("*.rs".into()),
            exclude: Some("gen.rs".into()),
            extended: true,
            ignore_case: true,
            context: Some(99),
        };
        let argv: Vec<String> = grep
            .argv()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv,
            [
                "-r",
                "-n",
                "-H",
                "-E",
                "-i",
                "-C20",
                "--include=*.rs",
                "--exclude=gen.rs",
                "-e",
                "-v",
                "--",
                "/w/a",
                "-x"
            ]
        );
        let sed = Sed {
            script: "s/a/b/g".into(),
            paths: vec!["/w/a/f".into()],
            extended: false,
        };
        let argv: Vec<String> = sed
            .argv()
            .unwrap()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv, ["--sandbox", "-i", "-e", "s/a/b/g", "--", "/w/a/f"]);
        assert!(Sed::default().argv().unwrap_err().contains("empty"));
    }

    /// td-txt is run under the applet's name, with the mapped argv: a
    /// stand-in that prints both shows it.
    #[test]
    fn td_txt_is_run_under_the_applets_name() {
        let dir = std::env::temp_dir().join(format!(
            "td-agent-txt-{}-{}",
            std::process::id(),
            crate::store::random_hex(4).unwrap()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stand_in = dir.join("td-txt");
        std::fs::write(&stand_in, "#!/bin/sh\necho \"$0\"; printf '<%s>' \"$@\"\n").unwrap();
        let mut mode = std::fs::metadata(&stand_in).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
        std::fs::set_permissions(&stand_in, mode).unwrap();
        // A sibling test forking while the write above was open hands its
        // child that descriptor until the child execs, and exec refuses a
        // file open for writing (ETXTBSY). No later fork can copy it, so
        // once one exec succeeds the stand-in stays runnable.
        let since = Instant::now();
        loop {
            match std::process::Command::new(&stand_in).output() {
                Err(e)
                    if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                        && since.elapsed() < Duration::from_secs(10) =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                other => {
                    other.unwrap();
                    break;
                }
            }
        }
        let never = AtomicBool::new(false);
        let grep = Grep {
            pattern: "x".into(),
            paths: vec![dir.clone()],
            ..Grep::default()
        };
        let exit = run(
            grep.command(&stand_in, &dir),
            Duration::from_secs(10),
            &never,
            &mut |_| {},
        )
        .unwrap();
        let text = exit.output.text();
        // A script sees its own path as $0; td-txt sees argv[0], which
        // `arg0` sets: the stand-in shows the argv, the arg0 test is
        // td-txt's own dispatch.
        assert!(
            text.contains(&format!("<-r><-n><-H><-e><x><--><{}>", dir.display())),
            "{text}"
        );
        assert_eq!(Grep::render(&exit).lines().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_says_no_matches_and_caps_its_lines() {
        let none = quick("exit 1");
        assert_eq!(Grep::render(&none), "[no matches]\n");
        let many = quick("seq 1 1500");
        let rendered = Grep::render(&many);
        assert_eq!(rendered.lines().count(), MAX_GREP_LINES + 1);
        assert!(rendered
            .ends_with("[500 more lines and 0 bytes not shown; narrow the pattern or the path]\n"));
        let failed = quick("echo 'grep: bad' >&2; exit 2");
        assert!(Grep::render(&failed).starts_with("[exit status 2]\ngrep: bad"));
    }
}
